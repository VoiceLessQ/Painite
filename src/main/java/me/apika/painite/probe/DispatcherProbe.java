package me.apika.painite.probe;

import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.LongAccumulator;

/** Counters for vanilla's ChunkTaskDispatcher pop-run-poll cycle, one set per dispatcher name. */
public final class DispatcherProbe {
	public static final DispatcherProbe WORLDGEN = new DispatcherProbe();
	public static final DispatcherProbe LIGHT = new DispatcherProbe();

	private final AtomicLong cycles = new AtomicLong();
	private final AtomicLong tasks = new AtomicLong();
	private final AtomicLong cycleNanos = new AtomicLong();
	private final AtomicLong runNanos = new AtomicLong();
	private final AtomicLong gapNanos = new AtomicLong();
	private final AtomicLong submits = new AtomicLong();
	private final AtomicLong levelChanges = new AtomicLong();
	private final LongAccumulator firstPop = new LongAccumulator(Math::min, Long.MAX_VALUE);
	private final LongAccumulator lastDone = new LongAccumulator(Math::max, 0);
	private volatile long lastDoneNanos;

	private DispatcherProbe() {}

	public static DispatcherProbe of(String name) {
		return "light".equals(name) ? LIGHT : WORLDGEN;
	}

	public void popped() {
		long now = System.nanoTime();
		long prev = lastDoneNanos;
		if (prev != 0) {
			gapNanos.addAndGet(now - prev);
		}
		firstPop.accumulate(now);
	}

	public void ran(long nanos) {
		tasks.incrementAndGet();
		runNanos.addAndGet(nanos);
	}

	public void cycleDone(long nanos) {
		long now = System.nanoTime();
		cycles.incrementAndGet();
		cycleNanos.addAndGet(nanos);
		lastDoneNanos = now;
		lastDone.accumulate(now);
	}

	public void submitted() {
		submits.incrementAndGet();
	}

	public void levelChanged() {
		levelChanges.incrementAndGet();
	}

	public static String report() {
		return "[painite] dispatcher probe\n" + WORLDGEN.line("worldgen") + LIGHT.line("light");
	}

	private String line(String name) {
		long c = cycles.get();
		long t = tasks.get();
		double span = c == 0 ? 0 : (lastDone.get() - firstPop.get()) / 1e9;
		return String.format("  %-9s cycles=%d tasks=%d submits=%d levelChanges=%d meanCycle=%.3fms meanRun=%.3fms meanGap=%.3fms span=%.1fs cycles/s=%.0f%n",
				name, c, t, submits.get(), levelChanges.get(),
				c == 0 ? 0 : cycleNanos.get() / 1e6 / c,
				t == 0 ? 0 : runNanos.get() / 1e6 / t,
				c == 0 ? 0 : gapNanos.get() / 1e6 / c,
				span, span == 0 ? 0 : c / span);
	}

	public static void reset() {
		for (DispatcherProbe p : new DispatcherProbe[] {WORLDGEN, LIGHT}) {
			p.cycles.set(0);
			p.tasks.set(0);
			p.cycleNanos.set(0);
			p.runNanos.set(0);
			p.gapNanos.set(0);
			p.submits.set(0);
			p.levelChanges.set(0);
			p.firstPop.reset();
			p.lastDone.reset();
			p.lastDoneNanos = 0;
		}
	}
}
