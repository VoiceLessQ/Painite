package me.apika.painite.lod;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import me.apika.painite.PainiteNative;
import me.apika.painite.terrain.TerrainBridge;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerTickEvents;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.server.level.ServerPlayer;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.chunk.status.ChunkStatus;
import net.minecraft.world.level.chunk.storage.SerializableChunkData;

/**
 * Gives a record to chunks that were generated before records existed (or by the
 * vanilla path): within each player's far radius, a chunk with no record whose file
 * says it is full is loaded at a low rate, and the chunk-load hook records it.
 */
public final class LodBackfill {
	/** Chunk loads per second for backfilling, -Dpainite.lodBackfill; 0 turns it off. */
	public static final int PER_SECOND = Integer.getInteger("painite.lodBackfill", 20);
	private static final int SCAN_PER_TICK = 64;
	private static final int READS_IN_FLIGHT = 32;
	private static final int CHECKED_LIMIT = 250_000;

	private static final class Walk {
		int centreX = Integer.MIN_VALUE;
		int centreZ = Integer.MIN_VALUE;
		int cursor;
		final Set<Long> checked = new HashSet<>();
	}

	/** Offsets within the far radius, nearest ring first. */
	private static List<int[]> offsets = List.of();
	private static final Map<ServerPlayer, Walk> WALKS = new ConcurrentHashMap<>();
	private static final ArrayDeque<ChunkPos> LOADS = new ArrayDeque<>();
	private static final AtomicInteger reads = new AtomicInteger();
	private static final AtomicLong loaded = new AtomicLong();
	private static final AtomicLong skipped = new AtomicLong();
	private static int budget;

	private LodBackfill() {}

	public static void register() {
		if (PER_SECOND <= 0) {
			return;
		}
		ServerTickEvents.END_SERVER_TICK.register(server -> tick(server.overworld()));
	}

	private static long key(int x, int z) {
		return ((long) x << 32) ^ (z & 0xffffffffL);
	}

	private static List<int[]> offsets(int radius) {
		if (offsets.size() != (2 * radius + 1) * (2 * radius + 1)) {
			List<int[]> out = new ArrayList<>();
			for (int r = 0; r <= radius; r++) {
				for (int dz = -r; dz <= r; dz++) {
					for (int dx = -r; dx <= r; dx++) {
						if (Math.max(Math.abs(dx), Math.abs(dz)) == r) {
							out.add(new int[] {dx, dz});
						}
					}
				}
			}
			offsets = out;
		}
		return offsets;
	}

	/** Server thread, once a tick. */
	private static void tick(ServerLevel level) {
		if (!TerrainBridge.lodActive()) {
			return;
		}
		List<ServerPlayer> players = LodSender.players();
		WALKS.keySet().retainAll(players);
		for (ServerPlayer player : players) {
			if (player.level().dimension() != Level.OVERWORLD) {
				continue;
			}
			scan(level, player, WALKS.computeIfAbsent(player, p -> new Walk()));
		}
		// Loads spread over the second; each blocks the tick for one chunk read.
		budget += PER_SECOND;
		while (budget >= 20 && !LOADS.isEmpty()) {
			budget -= 20;
			ChunkPos pos = LOADS.poll();
			if (PainiteNative.terrainLodStage(pos.x(), pos.z()) == 1) {
				continue;
			}
			level.getChunk(pos.x(), pos.z(), ChunkStatus.FULL, true);
			loaded.incrementAndGet();
		}
		if (LOADS.isEmpty()) {
			budget = Math.min(budget, 20);
		}
	}

	private static void scan(ServerLevel level, ServerPlayer player, Walk walk) {
		ChunkPos at = player.chunkPosition();
		if (Math.max(Math.abs(at.x() - walk.centreX), Math.abs(at.z() - walk.centreZ)) > 8) {
			walk.centreX = at.x();
			walk.centreZ = at.z();
			walk.cursor = 0;
		}
		if (walk.checked.size() > CHECKED_LIMIT) {
			walk.checked.clear();
		}
		List<int[]> disc = offsets(LodSender.RADIUS);
		int scanned = 0;
		while (scanned < SCAN_PER_TICK && walk.cursor < disc.size() && reads.get() < READS_IN_FLIGHT) {
			int[] d = disc.get(walk.cursor++);
			int x = walk.centreX + d[0];
			int z = walk.centreZ + d[1];
			scanned++;
			if (!walk.checked.add(key(x, z)) || PainiteNative.terrainLodStage(x, z) == 1) {
				continue;
			}
			ChunkPos pos = new ChunkPos(x, z);
			reads.incrementAndGet();
			level.getChunkSource().chunkMap.read(pos).whenComplete((tag, error) -> {
				reads.decrementAndGet();
				if (error == null && tag != null && tag.isPresent() && SerializableChunkData.getChunkStatusFromTag(tag.get()) == ChunkStatus.FULL) {
					level.getServer().execute(() -> LOADS.add(pos));
				} else {
					skipped.incrementAndGet();
				}
			});
		}
	}

	public static String report() {
		return PER_SECOND <= 0 ? "backfill off" : "backfill " + loaded.get() + " chunks loaded, " + skipped.get() + " not full on disk, " + LOADS.size() + " queued";
	}
}
