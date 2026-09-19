package me.apika.painite.probe;

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.LongAccumulator;

import net.minecraft.world.level.chunk.status.ChunkStatus;

/** Per-status pipeline view: latency from ChunkStep.apply to future completion, and how many overlap. */
public final class StepProbe {
	private static final StepProbe[] STEPS = new StepProbe[ChunkStatus.getStatusList().size()];

	static {
		for (int i = 0; i < STEPS.length; i++) {
			STEPS[i] = new StepProbe();
		}
	}

	private final AtomicInteger inFlight = new AtomicInteger();
	private final LongAccumulator maxInFlight = new LongAccumulator(Math::max, 0);
	private final AtomicLong jobs = new AtomicLong();
	private final AtomicLong latencyNanos = new AtomicLong();
	private final LongAccumulator firstStart = new LongAccumulator(Math::min, Long.MAX_VALUE);
	private final LongAccumulator lastEnd = new LongAccumulator(Math::max, 0);

	private StepProbe() {}

	public static StepProbe of(ChunkStatus status) {
		return STEPS[status.getIndex()];
	}

	public void started() {
		maxInFlight.accumulate(inFlight.incrementAndGet());
		firstStart.accumulate(System.nanoTime());
	}

	public void completed(long nanos) {
		inFlight.decrementAndGet();
		jobs.incrementAndGet();
		latencyNanos.addAndGet(nanos);
		lastEnd.accumulate(System.nanoTime());
	}

	public static String report() {
		StringBuilder sb = new StringBuilder("[painite] step probe (schedule to complete)\n");
		for (ChunkStatus status : ChunkStatus.getStatusList()) {
			StepProbe p = of(status);
			long n = p.jobs.get();
			if (n == 0) {
				continue;
			}
			double span = (p.lastEnd.get() - p.firstStart.get()) / 1e9;
			sb.append(String.format("  %-20s jobs=%d maxConcurrent=%d meanLatency=%.3fms span=%.1fs rate=%.1f/s%n",
					status.getName(), n, p.maxInFlight.get(), p.latencyNanos.get() / 1e6 / n, span, span == 0 ? 0 : n / span));
		}
		return sb.toString();
	}

	public static void reset() {
		for (StepProbe p : STEPS) {
			p.maxInFlight.reset();
			p.jobs.set(0);
			p.latencyNanos.set(0);
			p.firstStart.reset();
			p.lastEnd.reset();
		}
	}
}
