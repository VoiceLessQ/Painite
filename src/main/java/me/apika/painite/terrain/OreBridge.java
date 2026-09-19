package me.apika.painite.terrain;

import it.unimi.dsi.fastutil.ints.IntArrayList;
import it.unimi.dsi.fastutil.longs.LongArrayList;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicLong;
import me.apika.painite.PainiteMod;
import me.apika.painite.PainiteNative;
import io.netty.buffer.Unpooled;
import net.minecraft.core.BlockPos;
import net.minecraft.network.FriendlyByteBuf;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.Identifier;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.util.RandomSource;
import net.minecraft.world.level.WorldGenLevel;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.chunk.LevelChunkSection;
import net.minecraft.world.level.chunk.PalettedContainer;
import net.minecraft.world.level.levelgen.Heightmap;
import net.minecraft.world.level.levelgen.WorldgenRandom;
import net.minecraft.world.level.levelgen.feature.OreFeature;
import net.minecraft.world.level.levelgen.placement.FeaturePlacer;
import net.minecraft.world.level.levelgen.placement.PlacedFeature;

/**
 * Batches consecutive ore placed features of a chunk's decoration pass
 * and runs them natively over the 3x3 region's packed sections. Any
 * batch the native declines runs the vanilla features instead, so the
 * output only changes when the native accepted the whole batch.
 */
public final class OreBridge {
	/** -Dpainite.rustOres=true enables the native ore batch. */
	public static final boolean ENABLED = Boolean.getBoolean("painite.rustOres");
	/** -Dpainite.oreShadow=true runs both and reports differences; vanilla output is kept. */
	public static final boolean SHADOW = Boolean.getBoolean("painite.oreShadow");
	private static final int HEIGHT_LONGS = 37;
	private static final int SLOTS = 9;
	private static final int LOGGED_MISMATCHES = 8;

	public static final AtomicLong BATCHES = new AtomicLong();
	public static final AtomicLong FEATURES = new AtomicLong();
	public static final AtomicLong WRITES = new AtomicLong();
	public static final AtomicLong DECLINED = new AtomicLong();
	public static final AtomicLong SHADOW_BATCHES = new AtomicLong();
	public static final AtomicLong SHADOW_VANILLA_WRITES = new AtomicLong();
	public static final AtomicLong SHADOW_MISMATCHES = new AtomicLong();
	private static final AtomicLong logged = new AtomicLong();
	/** Nanoseconds per phase, summed over batches. */
	public static final AtomicLong NS_QUARTS = new AtomicLong();
	public static final AtomicLong NS_PACK = new AtomicLong();
	public static final AtomicLong NS_NATIVE = new AtomicLong();
	public static final AtomicLong NS_APPLY = new AtomicLong();

	/** Native placed index per placed feature instance, valid while the terrain is installed. */
	private static volatile Map<PlacedFeature, Integer> placedIndex = Map.of();
	private static volatile String[] placedNames = new String[0];
	/** Shadow statistics per placed feature: vanilla writes, mismatches. */
	private static final Map<String, long[]> SHADOW_BY_FEATURE = new java.util.concurrent.ConcurrentHashMap<>();
	private static final ThreadLocal<Batch> CURRENT = new ThreadLocal<>();

	private OreBridge() {}

	/** One chunk's open batch. */
	private static final class Batch {
		final WorldGenLevel level;
		final ChunkAccess chunk;
		FeaturePlacer placer;
		RandomSource random;
		BlockPos origin;
		final List<PlacedFeature> features = new ArrayList<>();
		final IntArrayList placed = new IntArrayList();
		final LongArrayList decorationSeeds = new LongArrayList();
		final IntArrayList indices = new IntArrayList();
		final IntArrayList steps = new IntArrayList();

		Batch(WorldGenLevel level, ChunkAccess chunk) {
			this.level = level;
			this.chunk = chunk;
		}
	}

	/** Maps the native's placed feature ids to registry instances; call after terrainInit. */
	public static void install(ServerLevel level, String[] placedIds) {
		if (placedIds == null) {
			placedIndex = Map.of();
			return;
		}
		Map<Identifier, Integer> byId = new HashMap<>();
		for (int i = 0; i < placedIds.length; i++) {
			byId.put(Identifier.parse(placedIds[i]), i);
		}
		Map<PlacedFeature, Integer> byInstance = new IdentityHashMap<>();
		level.registryAccess().lookupOrThrow(Registries.PLACED_FEATURE).listElements().forEach(holder -> {
			Integer i = byId.get(holder.key().identifier());
			if (i != null) {
				byInstance.put(holder.value(), i);
			}
		});
		placedIndex = byInstance;
		placedNames = placedIds.clone();
		PainiteMod.LOGGER.info("[painite] rustOres: {} placed ore features served{}", byInstance.size(), SHADOW ? " (shadow)" : "");
	}

	public static void clear() {
		placedIndex = Map.of();
	}

	/** HEAD of applyBiomeDecoration. */
	public static void begin(WorldGenLevel level, ChunkAccess chunk) {
		if (ENABLED && !placedIndex.isEmpty()) {
			CURRENT.set(new Batch(level, chunk));
		}
	}

	/** Every feature placement of the pass goes through here. */
	public static boolean place(FeaturePlacer placer, PlacedFeature feature, RandomSource random, BlockPos origin) {
		Batch batch = CURRENT.get();
		if (batch == null) {
			return placer.placeWithBiomeCheck(feature, random, origin);
		}
		Integer index = feature.feature().value() instanceof OreFeature ? placedIndex.get(feature) : null;
		if (index == null || !(random instanceof PainiteFeatureRandom seeded) || !(random instanceof WorldgenRandom)) {
			flush(batch);
			return placer.placeWithBiomeCheck(feature, random, origin);
		}
		batch.placer = placer;
		batch.random = random;
		batch.origin = origin;
		batch.features.add(feature);
		batch.placed.add(index.intValue());
		batch.decorationSeeds.add(seeded.painite$decorationSeed());
		batch.indices.add(seeded.painite$featureIndex());
		batch.steps.add(seeded.painite$featureStep());
		return true;
	}

	/** Before anything that is not a feature placement writes blocks. */
	public static void flush() {
		Batch batch = CURRENT.get();
		if (batch != null) {
			flush(batch);
		}
	}

	/** RETURN of applyBiomeDecoration. */
	public static void end() {
		Batch batch = CURRENT.get();
		if (batch != null) {
			flush(batch);
			CURRENT.remove();
		}
	}

	private static void flush(Batch batch) {
		if (batch.features.isEmpty()) {
			return;
		}
		try {
			if (!runNative(batch)) {
				DECLINED.incrementAndGet();
				runVanilla(batch);
			}
		} finally {
			batch.features.clear();
			batch.placed.clear();
			batch.decorationSeeds.clear();
			batch.indices.clear();
			batch.steps.clear();
		}
	}

	private static void runVanilla(Batch batch) {
		WorldgenRandom random = (WorldgenRandom) batch.random;
		for (int i = 0; i < batch.features.size(); i++) {
			random.setFeatureSeed(batch.decorationSeeds.getLong(i), batch.indices.getInt(i), batch.steps.getInt(i));
			batch.placer.placeWithBiomeCheck(batch.features.get(i), random, batch.origin);
		}
	}

	private static boolean runNative(Batch batch) {
		ChunkAccess chunk = batch.chunk;
		int chunkX = chunk.getPos().x();
		int chunkZ = chunk.getPos().z();
		long[] seeds = new long[batch.features.size()];
		for (int i = 0; i < seeds.length; i++) {
			seeds[i] = batch.decorationSeeds.getLong(i) + batch.indices.getInt(i) + 10000L * batch.steps.getInt(i);
		}
		int[] placed = batch.placed.toIntArray();
		long t1 = System.nanoTime();
		int range = PainiteNative.terrainOrePlan(chunkX, chunkZ, seeds, placed, null);
		long t2 = System.nanoTime();
		NS_NATIVE.addAndGet(t2 - t1);
		if (range < 0) {
			// No native biome output for a neighbour: read the grid here and plan again.
			TerrainBridge.GRID_FALLBACKS.incrementAndGet();
			int[] quarts = TerrainBridge.quartGrid(batch.level.getBiomeManager(), chunk);
			if (quarts == null) {
				return false;
			}
			long t3 = System.nanoTime();
			NS_QUARTS.addAndGet(t3 - t2);
			range = PainiteNative.terrainOrePlan(chunkX, chunkZ, seeds, placed, quarts);
			t2 = System.nanoTime();
			NS_NATIVE.addAndGet(t2 - t3);
			if (range < 0) {
				return false;
			}
		}
		int lo = range & 0xffff;
		int hi = range >>> 16;
		ChunkAccess[] chunks = new ChunkAccess[SLOTS];
		long[] heights = new long[SLOTS * HEIGHT_LONGS];
		for (int slot = 0; slot < SLOTS; slot++) {
			ChunkAccess c = batch.level.getChunk(chunkX + slot % 3 - 1, chunkZ + slot / 3 - 1);
			chunks[slot] = c;
			long[] raw = c.getOrCreateHeightmapUnprimed(Heightmap.Types.OCEAN_FLOOR_WG).getRawData();
			if (raw.length != HEIGHT_LONGS) {
				return false;
			}
			System.arraycopy(raw, 0, heights, slot * HEIGHT_LONGS, HEIGHT_LONGS);
		}
		IntArrayList meta = new IntArrayList();
		IntArrayList palettes = new IntArrayList();
		LongArrayList storage = new LongArrayList();
		FriendlyByteBuf buf = new FriendlyByteBuf(Unpooled.buffer(4096));
		for (int slot = 0; slot < SLOTS; slot++) {
			for (int s = lo; s <= hi; s++) {
				if (s >= chunks[slot].getSectionsCount()) {
					return false;
				}
				// The network form: bits, palette ids (none for the global palette), raw longs. No re-encoding.
				buf.clear();
				chunks[slot].getSection(s).getStates().write(buf);
				int bits = buf.readByte();
				int paletteOffset = palettes.size();
				int paletteLen;
				if (bits == 0) {
					paletteLen = 1;
					palettes.add(buf.readVarInt());
				} else if (bits <= 8) {
					paletteLen = buf.readVarInt();
					for (int i = 0; i < paletteLen; i++) {
						palettes.add(buf.readVarInt());
					}
				} else {
					paletteLen = 0;
				}
				int rawLen = buf.readableBytes() / 8;
				meta.add(slot | s << 4);
				meta.add(paletteLen);
				meta.add(bits);
				meta.add(paletteOffset);
				meta.add(storage.size());
				meta.add(rawLen);
				for (int i = 0; i < rawLen; i++) {
					storage.add(buf.readLong());
				}
			}
		}
		long t3 = System.nanoTime();
		NS_PACK.addAndGet(t3 - t2);
		int[] writes = PainiteNative.terrainOreApply(chunkX, chunkZ, heights, meta.toIntArray(), palettes.toIntArray(), storage.toLongArray());
		if (writes == null) {
			return false;
		}
		long t4 = System.nanoTime();
		NS_NATIVE.addAndGet(t4 - t3);
		BATCHES.incrementAndGet();
		FEATURES.addAndGet(batch.features.size());
		WRITES.addAndGet(writes.length / 2);
		if (SHADOW) {
			shadow(batch, chunks, lo, hi, writes);
		} else {
			apply(chunks, writes);
			NS_APPLY.addAndGet(System.nanoTime() - t4);
		}
		return true;
	}

	private static void apply(ChunkAccess[] chunks, int[] writes) {
		BlockState[] palette = TerrainBridge.paletteStates();
		for (int k = 0; k + 1 < writes.length; k += 2) {
			int w = writes[k];
			int packed = w >>> 16;
			LevelChunkSection section = chunks[w & 15].getSection((w >>> 4) & 0xfff);
			section.setBlockState(packed & 15, (packed >> 4) & 15, (packed >> 8) & 15, palette[writes[k + 1]], false);
		}
	}

	/** Runs vanilla on the live sections and compares its writes with the native's. */
	private static void shadow(Batch batch, ChunkAccess[] chunks, int lo, int hi, int[] writes) {
		BlockState[] palette = TerrainBridge.paletteStates();
		int span = hi - lo + 1;
		@SuppressWarnings("unchecked")
		PalettedContainer<BlockState>[] before = new PalettedContainer[SLOTS * span];
		for (int slot = 0; slot < SLOTS; slot++) {
			for (int s = lo; s <= hi; s++) {
				before[slot * span + s - lo] = chunks[slot].getSection(s).getStates().copy();
			}
		}
		runVanilla(batch);
		Map<Integer, Integer> nativeWrites = new HashMap<>();
		for (int k = 0; k + 1 < writes.length; k += 2) {
			nativeWrites.put(writes[k], writes[k + 1]);
		}
		long mismatches = 0;
		long vanillaWrites = 0;
		for (int slot = 0; slot < SLOTS; slot++) {
			for (int s = lo; s <= hi; s++) {
				PalettedContainer<BlockState> old = before[slot * span + s - lo];
				PalettedContainer<BlockState> now = chunks[slot].getSection(s).getStates();
				for (int y = 0; y < 16; y++) {
					for (int z = 0; z < 16; z++) {
						for (int x = 0; x < 16; x++) {
							BlockState a = old.get(x, y, z);
							BlockState b = now.get(x, y, z);
							int key = slot | s << 4 | (x | y << 4 | z << 8) << 16;
							Integer n = nativeWrites.remove(key);
							if (a != b) {
								vanillaWrites++;
							}
							// A native write must leave the block vanilla left; a vanilla change needs a native write.
							BlockState got = n == null ? null : palette[n];
							boolean bad = n == null ? a != b : got != b;
							if (bad) {
								mismatches++;
								log(batch, chunks[slot], s, x, y, z, a == b ? null : b, got);
							}
						}
					}
				}
			}
		}
		for (Map.Entry<Integer, Integer> extra : nativeWrites.entrySet()) {
			int w = extra.getKey();
			int packed = w >>> 16;
			mismatches++;
			log(batch, chunks[w & 15], (w >>> 4) & 0xfff, packed & 15, (packed >> 4) & 15, (packed >> 8) & 15, null, palette[extra.getValue()]);
		}
		SHADOW_BATCHES.incrementAndGet();
		SHADOW_VANILLA_WRITES.addAndGet(vanillaWrites);
		SHADOW_MISMATCHES.addAndGet(mismatches);
		if (batch.features.size() == 1) {
			long[] stat = SHADOW_BY_FEATURE.computeIfAbsent(placedNames[batch.placed.getInt(0)], k -> new long[2]);
			synchronized (stat) {
				stat[0] += vanillaWrites;
				stat[1] += mismatches;
			}
		}
	}

	private static void log(Batch batch, ChunkAccess chunk, int section, int x, int y, int z, BlockState vanilla, BlockState nativeState) {
		if (logged.incrementAndGet() > LOGGED_MISMATCHES) {
			return;
		}
		int blockY = chunk.getMinY() + section * 16 + y;
		PainiteMod.LOGGER.warn("[painite] oreShadow: chunk {} at ({}, {}, {}) vanilla={} native={} feature={}", batch.chunk.getPos(),
				chunk.getPos().x() * 16 + x, blockY, chunk.getPos().z() * 16 + z, vanilla, nativeState, placedNames[batch.placed.getInt(0)]);
	}

	public static String report() {
		long batches = Math.max(1, BATCHES.get());
		String base = "  ores batches=" + BATCHES.get() + " features=" + FEATURES.get() + " writes=" + WRITES.get() + " declined=" + DECLINED.get()
				+ String.format(" ms/batch quarts=%.3f pack=%.3f native=%.3f apply=%.3f", NS_QUARTS.get() / 1e6 / batches, NS_PACK.get() / 1e6 / batches,
						NS_NATIVE.get() / 1e6 / batches, NS_APPLY.get() / 1e6 / batches);
		if (SHADOW) {
			base += " shadowBatches=" + SHADOW_BATCHES.get() + " vanillaWrites=" + SHADOW_VANILLA_WRITES.get() + " mismatches=" + SHADOW_MISMATCHES.get();
			StringBuilder sb = new StringBuilder(base);
			SHADOW_BY_FEATURE.entrySet().stream().sorted(Map.Entry.comparingByKey()).forEach(e -> {
				long[] stat = e.getValue();
				sb.append("\n    ").append(e.getKey()).append(" vanilla=").append(stat[0]).append(" mismatches=").append(stat[1]);
			});
			base = sb.toString();
		}
		return base + "\n";
	}
}
