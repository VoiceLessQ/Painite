package me.apika.painite.probe;

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.LongAccumulator;

/** Per-stage counters for the gate. Lock-free, always on; cost is a few atomics per chunk-stage. */
public final class StageProbe {
	public static final String[] NAMES = {"starts", "references", "features", "spawn"};
	private static final StageProbe[] STAGES = {new StageProbe(), new StageProbe(), new StageProbe(), new StageProbe()};

	private final AtomicInteger inFlight = new AtomicInteger();
	private final LongAccumulator maxInFlight = new LongAccumulator(Math::max, 0);
	private final AtomicLong jobs = new AtomicLong();
	private final AtomicLong waitNanos = new AtomicLong();
	private final AtomicLong runNanos = new AtomicLong();
	private final LongAccumulator firstStart = new LongAccumulator(Math::min, Long.MAX_VALUE);
	private final LongAccumulator lastEnd = new LongAccumulator(Math::max, 0);

	private StageProbe() {}

	public static StageProbe of(int stage) {
		return STAGES[stage];
	}

	public void granted(long waitedNanos) {
		waitNanos.addAndGet(waitedNanos);
		int now = inFlight.incrementAndGet();
		maxInFlight.accumulate(now);
		firstStart.accumulate(System.nanoTime());
	}

	public void finished(long ranNanos) {
		inFlight.decrementAndGet();
		runNanos.addAndGet(ranNanos);
		jobs.incrementAndGet();
		lastEnd.accumulate(System.nanoTime());
	}

	public static String report() {
		StringBuilder sb = new StringBuilder("[painite] stage probe\n");
		for (int i = 0; i < STAGES.length; i++) {
			StageProbe p = STAGES[i];
			long n = p.jobs.get();
			double span = n == 0 ? 0 : (p.lastEnd.get() - p.firstStart.get()) / 1e9;
			sb.append(String.format("  %-10s jobs=%d maxConcurrent=%d meanWait=%.3fms meanRun=%.3fms span=%.1fs rate=%.1f/s%n",
					NAMES[i], n, p.maxInFlight.get(),
					n == 0 ? 0 : p.waitNanos.get() / 1e6 / n,
					n == 0 ? 0 : p.runNanos.get() / 1e6 / n,
					span, span == 0 ? 0 : n / span));
		}
		return sb.toString();
	}

	public static void reset() {
		for (StageProbe p : STAGES) {
			p.maxInFlight.reset();
			p.jobs.set(0);
			p.waitNanos.set(0);
			p.runNanos.set(0);
			p.firstStart.reset();
			p.lastEnd.reset();
		}
	}
}
