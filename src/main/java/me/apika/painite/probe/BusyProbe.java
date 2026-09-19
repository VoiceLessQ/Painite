package me.apika.painite.probe;

import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.LongAccumulator;
import java.util.concurrent.atomic.LongAdder;

/** Named busy-time counters for methods whose JFR share needs a ground truth. */
public final class BusyProbe {
	private static final Map<String, BusyProbe> PROBES = new ConcurrentHashMap<>();

	private final LongAdder calls = new LongAdder();
	private final LongAdder nanos = new LongAdder();
	private final LongAccumulator firstStart = new LongAccumulator(Math::min, Long.MAX_VALUE);
	private final LongAccumulator lastEnd = new LongAccumulator(Math::max, 0);

	private BusyProbe() {}

	public static BusyProbe of(String name) {
		return PROBES.computeIfAbsent(name, n -> new BusyProbe());
	}

	public long start() {
		long now = System.nanoTime();
		firstStart.accumulate(now);
		return now;
	}

	public void stop(long t0) {
		long now = System.nanoTime();
		calls.increment();
		nanos.add(now - t0);
		lastEnd.accumulate(now);
	}

	public static String report() {
		StringBuilder sb = new StringBuilder("[painite] busy probes\n");
		PROBES.entrySet().stream().sorted(Map.Entry.comparingByKey()).forEach(e -> {
			BusyProbe p = e.getValue();
			long n = p.calls.sum();
			double busy = p.nanos.sum() / 1e9;
			double span = n == 0 ? 0 : (p.lastEnd.get() - p.firstStart.get()) / 1e9;
			sb.append(String.format("  %-22s calls=%d busy=%.2fs span=%.1fs mean=%.3fms%n", e.getKey(), n, busy, span, n == 0 ? 0 : busy * 1e3 / n));
		});
		return sb.toString();
	}

	public static void reset() {
		for (BusyProbe p : PROBES.values()) {
			p.calls.reset();
			p.nanos.reset();
			p.firstStart.reset();
			p.lastEnd.reset();
		}
	}
}
