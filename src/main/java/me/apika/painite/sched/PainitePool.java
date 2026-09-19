package me.apika.painite.sched;

import java.util.concurrent.Executor;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.atomic.AtomicInteger;

/** Threads that run gated stage bodies. Sized like the Rust limits. */
public final class PainitePool {
	private static final int WORKERS = computeWorkers();
	private static final ExecutorService POOL = Executors.newFixedThreadPool(WORKERS, factory());

	private PainitePool() {}

	public static int workerCount() {
		return WORKERS;
	}

	public static Executor executor() {
		return POOL;
	}

	// Mirrors painite_sched::Limits::for_cores: clamp(cores - 1, 1, 16).
	private static int computeWorkers() {
		String override = System.getProperty("painite.workers");
		if (override != null) {
			try {
				return Math.max(1, Integer.parseInt(override.trim()));
			} catch (NumberFormatException ignored) {
				// fall through to the derived value
			}
		}
		int cores = Runtime.getRuntime().availableProcessors();
		return Math.clamp(cores - 1, 1, 16);
	}

	private static ThreadFactory factory() {
		AtomicInteger n = new AtomicInteger(1);
		return r -> {
			Thread t = new Thread(r, "Painite-Worker-" + n.getAndIncrement());
			t.setDaemon(true);
			return t;
		};
	}
}
