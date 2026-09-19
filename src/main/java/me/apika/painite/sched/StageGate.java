package me.apika.painite.sched;

import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.function.Supplier;

import me.apika.painite.PainiteMod;
import me.apika.painite.PainiteNative;
import me.apika.painite.probe.StageProbe;
import net.minecraft.server.level.GenerationChunkHolder;
import net.minecraft.util.StaticCache2D;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.chunk.ChunkAccess;

/**
 * Runs a serial-stage body on the Painite pool once the native scheduler
 * admits it. Stage ids match painite_sched::Stage; never reorder.
 */
public final class StageGate {
	public static final int STRUCTURE_STARTS = 0;
	public static final int STRUCTURE_REFERENCES = 1;
	public static final int FEATURES = 2;
	public static final int SPAWN = 3;

	/** Default on; -Dpainite.parallelFeatures=false keeps FEATURES on the serial executor. */
	public static final boolean PARALLEL_FEATURES = Boolean.parseBoolean(System.getProperty("painite.parallelFeatures", "true"));
	public static final boolean PARALLEL_STRUCTURES = Boolean.getBoolean("painite.parallelStructures");

	private StageGate() {}

	public static boolean enabled(int stage) {
		if (!PainiteNative.AVAILABLE) {
			return false;
		}
		return stage == FEATURES ? PARALLEL_FEATURES : PARALLEL_STRUCTURES;
	}

	/** Ungated path: run inline on the caller, only the probe sees it. */
	public static CompletableFuture<ChunkAccess> timed(int stage, Supplier<CompletableFuture<ChunkAccess>> body) {
		StageProbe probe = StageProbe.of(stage);
		long t0 = System.nanoTime();
		probe.granted(0);
		try {
			return body.get();
		} finally {
			probe.finished(System.nanoTime() - t0);
		}
	}

	/** Parked bodies by seq; guarded by GATE so a grant never races its own submit. */
	private static final Map<Long, Runnable> PARKED = new HashMap<>();
	private static final Object GATE = new Object();

	/**
	 * Non-blocking gate: the job is registered in the native scheduler and
	 * runs on the pool only once admitted, so no worker ever waits on a
	 * zone. Grants arrive from the releasing worker.
	 */
	public static CompletableFuture<ChunkAccess> run(
			int stage,
			StaticCache2D<GenerationChunkHolder> chunks,
			ChunkAccess chunk,
			Supplier<CompletableFuture<ChunkAccess>> body) {
		ChunkPos pos = chunk.getPos();
		int level = chunks.get(pos.x(), pos.z()).getQueueLevel();
		StageProbe probe = StageProbe.of(stage);
		CompletableFuture<ChunkAccess> result = new CompletableFuture<>();
		long t0 = System.nanoTime();
		long[] seqHolder = new long[1];
		Runnable job = () -> {
			long t1 = System.nanoTime();
			probe.granted(t1 - t0);
			long[] freed;
			try {
				result.complete(body.get().join());
			} catch (Throwable t) {
				result.completeExceptionally(t);
			} finally {
				freed = PainiteNative.release(stage, pos.x(), pos.z(), seqHolder[0]);
				probe.finished(System.nanoTime() - t1);
			}
			dispatch(freed);
		};
		synchronized (GATE) {
			long r = PainiteNative.submit(stage, pos.x(), pos.z(), level);
			if (r < 0) {
				seqHolder[0] = -1;
				PainitePool.executor().execute(job);
				return result;
			}
			seqHolder[0] = r >>> 1;
			if ((r & 1) != 0) {
				PainitePool.executor().execute(job);
			} else {
				PARKED.put(seqHolder[0], job);
			}
		}
		return result;
	}

	private static void dispatch(long[] freed) {
		if (freed == null || freed.length == 0) {
			return;
		}
		synchronized (GATE) {
			for (long seq : freed) {
				Runnable job = PARKED.remove(seq);
				if (job == null) {
					PainiteMod.LOGGER.error("[painite] granted seq {} has no parked job", seq);
					continue;
				}
				PainitePool.executor().execute(job);
			}
		}
	}
}
