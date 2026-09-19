package me.apika.painite.terrain;

import com.google.gson.JsonArray;
import com.google.gson.JsonElement;
import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import com.mojang.serialization.DataResult;
import com.mojang.serialization.JsonOps;
import it.unimi.dsi.fastutil.shorts.ShortArrayList;
import it.unimi.dsi.fastutil.shorts.ShortList;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.atomic.AtomicLong;
import java.util.stream.LongStream;
import me.apika.painite.PainiteMod;
import me.apika.painite.PainiteNative;
import me.apika.painite.mixin.BeardifierAccessor;
import me.apika.painite.probe.FeatureProbe;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerLevelEvents;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents;
import net.minecraft.core.BlockPos;
import net.minecraft.core.Holder;
import net.minecraft.core.QuartPos;
import net.minecraft.core.SectionPos;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.core.registries.Registries;
import net.minecraft.commands.arguments.blocks.BlockStateParser;
import net.minecraft.resources.Identifier;
import net.minecraft.resources.RegistryOps;
import net.minecraft.resources.ResourceKey;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.tags.BlockTags;
import net.minecraft.world.level.Level;
import net.minecraft.world.level.biome.Biome;
import net.minecraft.world.level.biome.BiomeManager;
import net.minecraft.world.level.biome.BiomeResolver;
import net.minecraft.world.level.biome.Climate;
import net.minecraft.world.level.biome.MultiNoiseBiomeSource;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.chunk.ChunkGenerator;
import net.minecraft.world.level.chunk.LevelChunkSection;
import net.minecraft.world.level.chunk.PalettedContainer;
import net.minecraft.world.level.chunk.PalettedContainerRO;
import net.minecraft.world.level.chunk.Strategy;
import net.minecraft.world.level.levelgen.Beardifier;
import net.minecraft.world.level.levelgen.carver.WorldCarver;
import net.minecraft.world.level.levelgen.Heightmap;
import net.minecraft.world.level.levelgen.structure.BoundingBox;
import net.minecraft.world.level.storage.LevelResource;
import net.minecraft.world.level.levelgen.structure.pools.JigsawJunction;
import net.minecraft.world.level.levelgen.feature.Feature;
import net.minecraft.world.level.levelgen.feature.OreFeature;
import net.minecraft.world.level.levelgen.placement.PlacedFeature;
import net.minecraft.world.level.levelgen.NoiseBasedChunkGenerator;
import net.minecraft.world.level.levelgen.NoiseGeneratorSettings;
import net.minecraft.world.level.levelgen.densityfunction.DensityFunction;
import net.minecraft.world.level.levelgen.densityfunction.DensityFunctions;
import net.minecraft.world.level.levelgen.densityfunction.DensityVolume;
import net.minecraft.world.level.levelgen.material.condition.MaterialCondition;
import net.minecraft.world.level.levelgen.material.rule.MaterialRule;
import net.minecraft.world.level.levelgen.synth.NormalNoise;

/**
 * Hands the overworld's worldgen documents to the native at world load
 * so the TERRAIN fill can run there. Any world the native cannot serve
 * keeps the vanilla path; nothing here changes output.
 */
public final class TerrainBridge {
	/** Default on; -Dpainite.rustTerrain=false keeps the vanilla fill. */
	public static final boolean ENABLED = Boolean.parseBoolean(System.getProperty("painite.rustTerrain", "true"));
	/** Default on; -Dpainite.rustCarvers=false leaves the carvers to the game on natively surfaced chunks. */
	public static final boolean CARVERS = Boolean.parseBoolean(System.getProperty("painite.rustCarvers", "true"));
	/** Far view, default off: -Dpainite.lod=true keeps column records, sends them, and lets the client store and draw them. */
	public static final boolean LOD = Boolean.parseBoolean(System.getProperty("painite.lod", "false"));
	/** Whether the compiled terrain can carve, asked once after terrainInit. */
	private static volatile boolean nativeCarvers;

	private static volatile ResourceKey<NoiseGeneratorSettings> activeSettings;
	/** Biome holder per native biome index (registry order), valid while activeSettings is set. */
	private static volatile Holder<Biome>[] biomeHolders = new Holder[0];
	/** Block state per native palette id, valid while activeSettings is set. */
	private static volatile BlockState[] palette = new BlockState[0];
	/** Biome registry index per biome id, the order the native was given. */
	private static volatile Map<Identifier, Integer> biomeIndex = Map.of();
	public static final AtomicLong NATIVE_FILLS = new AtomicLong();
	public static final AtomicLong VANILLA_FILLS = new AtomicLong();
	public static final AtomicLong INELIGIBLE_FILLS = new AtomicLong();
	public static final AtomicLong BEARD_FILLS = new AtomicLong();
	public static final AtomicLong NATIVE_SURFACES = new AtomicLong();
	public static final AtomicLong NATIVE_BIOMES = new AtomicLong();
	public static final AtomicLong SURFACE_FALLBACKS = new AtomicLong();
	/** Passes that had to read the quart grid from the game because the native lacked a neighbour's biomes. */
	public static final AtomicLong GRID_FALLBACKS = new AtomicLong();
	/** Chunks whose carvers ran in the native surface pass. */
	public static final AtomicLong NATIVE_CARVES = new AtomicLong();

	private TerrainBridge() {}

	public static void register() {
		if (!ENABLED) {
			return;
		}
		if (!PainiteNative.AVAILABLE) {
			PainiteMod.LOGGER.warn("[painite] rustTerrain requested but the native is unavailable");
			return;
		}
		ServerLevelEvents.LOAD.register((server, level) -> onLoad(level));
		ServerLifecycleEvents.BEFORE_SAVE.register((server, flush, force) -> flushLod());
		ServerLevelEvents.UNLOAD.register((server, level) -> {
			if (level.dimension() == Level.OVERWORLD) {
				flushLod();
				activeSettings = null;
				palette = new BlockState[0];
				biomeIndex = Map.of();
				biomeHolders = new Holder[0];
				OreBridge.clear();
				PainiteNative.terrainClear();
			}
		});
	}

	/** True when this generator's settings are the ones compiled in the native. */
	public static boolean serves(NoiseBasedChunkGenerator generator) {
		ResourceKey<NoiseGeneratorSettings> active = activeSettings;
		return active != null && generator.generatorSettings().unwrapKey().map(active::equals).orElse(false);
	}

	private static void onLoad(ServerLevel level) {
		if (level.dimension() != Level.OVERWORLD) {
			return;
		}
		FeatureProbe.install(level);
		ChunkGenerator generator = level.getChunkSource().getGenerator();
		if (!(generator instanceof NoiseBasedChunkGenerator noiseGenerator)) {
			PainiteMod.LOGGER.info("[painite] rustTerrain: overworld generator is {}, vanilla path", generator.getClass().getSimpleName());
			return;
		}
		Holder<NoiseGeneratorSettings> settings = noiseGenerator.generatorSettings();
		ResourceKey<NoiseGeneratorSettings> key = settings.unwrapKey().orElse(null);
		if (key == null) {
			PainiteMod.LOGGER.info("[painite] rustTerrain: inline noise settings, vanilla path");
			return;
		}
		RegistryOps<JsonElement> ops = RegistryOps.create(JsonOps.INSTANCE, level.registryAccess());
		List<String> kinds = new ArrayList<>();
		List<String> ids = new ArrayList<>();
		List<String> bodies = new ArrayList<>();
		long start = System.nanoTime();
		try {
			add(kinds, ids, bodies, "noise_settings", key.identifier().toString(),
					NoiseGeneratorSettings.DIRECT_CODEC.encodeStart(ops, settings.value()));
			level.registryAccess().lookupOrThrow(Registries.DENSITY_FUNCTION).listElements().forEach(holder ->
					add(kinds, ids, bodies, "density_function", holder.key().identifier().toString(),
							DensityFunctions.DIRECT_CODEC.encodeStart(ops, holder.value())));
			level.registryAccess().lookupOrThrow(Registries.NOISE).listElements().forEach(holder ->
					add(kinds, ids, bodies, "noise", holder.key().identifier().toString(),
							NormalNoise.DIRECT_CODEC.encodeStart(ops, holder.value())));
			level.registryAccess().lookupOrThrow(Registries.MATERIAL_RULE).listElements().forEach(holder ->
					add(kinds, ids, bodies, "material_rule", holder.key().identifier().toString(),
							MaterialRule.DIRECT_CODEC.encodeStart(ops, holder.value())));
			level.registryAccess().lookupOrThrow(Registries.MATERIAL_CONDITION).listElements().forEach(holder ->
					add(kinds, ids, bodies, "material_condition", holder.key().identifier().toString(),
							MaterialCondition.DIRECT_CODEC.encodeStart(ops, holder.value())));
			Map<Identifier, Integer> biomes = new HashMap<>();
			level.registryAccess().lookupOrThrow(Registries.BIOME).listElements().forEach(holder -> {
				biomes.put(holder.key().identifier(), biomes.size());
				add(kinds, ids, bodies, "biome", holder.key().identifier().toString(),
						Biome.DIRECT_CODEC.encodeStart(ops, holder.value()));
			});
			biomeIndex = Map.copyOf(biomes);
			Holder<Biome>[] holders = biomeHoldersInOrder(level);
			String parameters = biomeParameters(level, noiseGenerator, ops);
			if (parameters != null) {
				kinds.add("biome_parameters");
				ids.add("minecraft:overworld");
				bodies.add(parameters);
			}
			biomeHolders = holders;
			level.registryAccess().lookupOrThrow(Registries.FEATURE).listElements().forEach(holder -> {
				if (holder.value() instanceof OreFeature) {
					add(kinds, ids, bodies, "feature", holder.key().identifier().toString(), Feature.DIRECT_CODEC.encodeStart(ops, holder.value()));
				}
			});
			level.registryAccess().lookupOrThrow(Registries.PLACED_FEATURE).listElements().forEach(holder -> {
				if (holder.value().feature().value() instanceof OreFeature) {
					add(kinds, ids, bodies, "placed_feature", holder.key().identifier().toString(), PlacedFeature.DIRECT_CODEC.encodeStart(ops, holder.value()));
				}
			});
			level.registryAccess().lookupOrThrow(Registries.CARVER).listElements().forEach(holder ->
					add(kinds, ids, bodies, "carver", holder.key().identifier().toString(), WorldCarver.DIRECT_CODEC.encodeStart(ops, holder.value())));
			kinds.add("block_index");
			ids.add("minecraft:overworld");
			bodies.add(blockIndex().toString());
		} catch (IllegalStateException e) {
			PainiteMod.LOGGER.warn("[painite] rustTerrain: could not encode worldgen documents, vanilla path: {}", e.getMessage());
			return;
		}
		long biomeZoomSeed = BiomeManager.obfuscateSeed(level.getSeed());
		int result = PainiteNative.terrainInit(level.getSeed(), biomeZoomSeed, key.identifier().toString(),
				kinds.toArray(String[]::new), ids.toArray(String[]::new), bodies.toArray(String[]::new));
		if (result != 1) {
			activeSettings = null;
			PainiteMod.LOGGER.info("[painite] rustTerrain: native declined {} (code {}), vanilla path", key.identifier(), result);
			return;
		}
		String[] names = PainiteNative.terrainPalette();
		if (names == null || !installPalette(names, ops)) {
			activeSettings = null;
			PainiteNative.terrainClear();
			return;
		}
		OreBridge.install(level, PainiteNative.terrainOrePlaced());
		nativeCarvers = PainiteNative.terrainCarvers() == 1;
		worldId = worldId(level.getSeed(), key.identifier().toString());
		if (LOD) {
			String dir = level.getServer().getWorldPath(LevelResource.ROOT).resolve("painite").resolve("lod").toAbsolutePath().toString();
			if (PainiteNative.terrainLodDir(dir) != 1) {
				PainiteMod.LOGGER.warn("[painite] far-view records: cannot create {}, keeping them in memory only", dir);
			}
			me.apika.painite.lod.LodPalette.load(java.nio.file.Path.of(dir));
		}
		long ms = (System.nanoTime() - start) / 1_000_000L;
		activeSettings = key;
		PainiteMod.LOGGER.info("[painite] rustTerrain active for {} ({} documents, {} palette states, carvers {}, {} ms)",
				key.identifier(), ids.size(), names.length, nativeCarvers ? (CARVERS ? "native" : "off") : "vanilla", ms);
	}

	@SuppressWarnings("unchecked")
	private static Holder<Biome>[] biomeHoldersInOrder(ServerLevel level) {
		List<Holder<Biome>> list = new ArrayList<>();
		level.registryAccess().lookupOrThrow(Registries.BIOME).listElements().forEach(list::add);
		return list.toArray(new Holder[0]);
	}

	/**
	 * The multi-noise parameter list as the game holds it (quantized longs),
	 * as JSON for the native, or null when the biome source is not multi-noise.
	 */
	public static String biomeParameters(ServerLevel level, NoiseBasedChunkGenerator generator, RegistryOps<JsonElement> ops) {
		if (!(generator.getBiomeSource() instanceof MultiNoiseBiomeSource source)) {
			return null;
		}
		JsonElement encoded = MultiNoiseBiomeSource.CODEC.codec().encodeStart(ops, source)
				.getOrThrow(msg -> new IllegalStateException("biome source: " + msg));
		Climate.ParameterList<Holder<Biome>> list;
		JsonObject obj = encoded.getAsJsonObject();
		if (obj.has("preset")) {
			Identifier presetId = Identifier.parse(obj.get("preset").getAsString());
			list = level.registryAccess().lookupOrThrow(Registries.MULTI_NOISE_BIOME_SOURCE_PARAMETER_LIST)
					.getOrThrow(ResourceKey.create(Registries.MULTI_NOISE_BIOME_SOURCE_PARAMETER_LIST, presetId)).value().parameters();
		} else {
			list = Climate.ParameterList.codec(Biome.CODEC.fieldOf("biome")).parse(ops, obj.get("biomes"))
					.getOrThrow(msg -> new IllegalStateException("biome list: " + msg));
		}
		StringBuilder out = new StringBuilder("{\"values\":[");
		boolean firstEntry = true;
		for (var pair : list.values()) {
			Climate.ParameterPoint p = pair.getFirst();
			String id = pair.getSecond().unwrapKey().map(k -> k.identifier().toString()).orElse(null);
			if (id == null) {
				return null;
			}
			if (!firstEntry) {
				out.append(',');
			}
			firstEntry = false;
			out.append("{\"biome\":\"").append(id).append("\",\"space\":[")
					.append(p.temperature().min()).append(',').append(p.temperature().max()).append(',')
					.append(p.humidity().min()).append(',').append(p.humidity().max()).append(',')
					.append(p.continentalness().min()).append(',').append(p.continentalness().max()).append(',')
					.append(p.erosion().min()).append(',').append(p.erosion().max()).append(',')
					.append(p.depth().min()).append(',').append(p.depth().max()).append(',')
					.append(p.weirdness().min()).append(',').append(p.weirdness().max()).append(',')
					.append(p.offset()).append("]}");
		}
		return out.append("]}").toString();
	}

	/**
	 * Fills a chunk's biome containers from the native biome stage. Returns
	 * false when the native has no biome stage for this world.
	 */
	public static boolean fillBiomes(ChunkAccess chunk) {
		int[] quarts = PainiteNative.terrainBiomes(chunk.getPos().x(), chunk.getPos().z());
		int quartCount = QuartPos.fromBlock(chunk.getHeight());
		if (quarts == null || quarts.length != 16 * quartCount) {
			return false;
		}
		Holder<Biome>[] holders = biomeHolders;
		for (int q : quarts) {
			if (q < 0 || q >= holders.length) {
				return false;
			}
		}
		int quartMinX = QuartPos.fromBlock(chunk.getPos().getMinBlockX());
		int quartMinY = QuartPos.fromBlock(chunk.getMinY());
		int quartMinZ = QuartPos.fromBlock(chunk.getPos().getMinBlockZ());
		BiomeResolver resolver = (qx, qy, qz) -> holders[quarts[(qy - quartMinY) + ((qx - quartMinX) + (qz - quartMinZ) * 4) * quartCount]];
		chunk.fillBiomesFromNoise(resolver);
		NATIVE_BIOMES.incrementAndGet();
		return true;
	}

	/** Decodes the native's palette and hands back the flags its heightmaps and stone test need. */
	private static boolean installPalette(String[] names, RegistryOps<JsonElement> ops) {
		BlockState[] states = new BlockState[names.length];
		byte[] flags = new byte[names.length];
		for (int i = 0; i < names.length; i++) {
			JsonElement json = JsonParser.parseString(names[i]);
			DataResult<BlockState> parsed = BlockState.CODEC.parse(ops, json);
			if (parsed.isError()) {
				PainiteMod.LOGGER.warn("[painite] rustTerrain: palette state {} unknown, vanilla path: {}", names[i], parsed.error().map(Object::toString).orElse(""));
				return false;
			}
			BlockState state = parsed.getOrThrow();
			states[i] = state;
			int f = 0;
			if (state.isAir()) {
				f |= 1;
			}
			if (!state.getFluidState().isEmpty()) {
				f |= 2;
			}
			if (state.is(BlockTags.BLOCKS_MOTION_IN_HEIGHTMAP)) {
				f |= 4;
			}
			flags[i] = (byte) f;
		}
		if (PainiteNative.terrainPaletteFlags(flags) != 1) {
			PainiteMod.LOGGER.warn("[painite] rustTerrain: native rejected palette flags, vanilla path");
			return false;
		}
		palette = states;
		return true;
	}

	/**
	 * The biome quart grid read from the game for a pass the native could
	 * not serve from its own biome output, or null when a biome is unknown to it.
	 */
	public static int[] quartGrid(BiomeManager biomeManager, ChunkAccess chunk) {
		Map<Identifier, Integer> index = biomeIndex;
		int quartMinY = QuartPos.fromBlock(chunk.getMinY());
		int quartCount = QuartPos.fromBlock(chunk.getHeight());
		int quartX0 = chunk.getPos().x() * 4 - 1;
		int quartZ0 = chunk.getPos().z() * 4 - 1;
		int[] quarts = new int[6 * 6 * quartCount];
		for (int qz = 0; qz < 6; qz++) {
			for (int qx = 0; qx < 6; qx++) {
				for (int qy = 0; qy < quartCount; qy++) {
					Holder<Biome> biome = biomeManager.getNoiseBiomeAtQuart(quartX0 + qx, quartMinY + qy, quartZ0 + qz);
					Integer i = biome.unwrapKey().map(k -> index.get(k.identifier())).orElse(null);
					if (i == null) {
						return null;
					}
					quarts[qy + (qx + qz * 6) * quartCount] = i;
				}
			}
		}
		return quarts;
	}

	/**
	 * Writes a native block array (palette id | 0x80 post-process per block,
	 * fill order) into the chunk's sections and worldgen heightmaps.
	 */
	public static void writeBlocks(ChunkAccess chunk, DensityVolume volume, byte[] data) {
		BlockState[] states = palette;
		Heightmap oceanFloor = chunk.getOrCreateHeightmapUnprimed(Heightmap.Types.OCEAN_FLOOR_WG);
		Heightmap worldSurface = chunk.getOrCreateHeightmapUnprimed(Heightmap.Types.WORLD_SURFACE_WG);
		BlockPos.MutableBlockPos pos = new BlockPos.MutableBlockPos();
		for (int z = 0; z < volume.sizeZ(); z++) {
			int blockZ = volume.blockZ(z);
			for (int x = 0; x < volume.sizeX(); x++) {
				int blockX = volume.blockX(x);
				for (int y = volume.sizeY() - 1; y >= 0; y--) {
					int code = data[volume.indexUnchecked(x, y, z)];
					BlockState state = states[code & 0x7f];
					if (state.isAir()) {
						continue;
					}
					int blockY = volume.blockY(y);
					LevelChunkSection section = chunk.getSection(chunk.getSectionIndex(blockY));
					section.setBlockState(x, SectionPos.sectionRelative(blockY), z, state, false);
					oceanFloor.update(x, blockY, z, state);
					worldSurface.update(x, blockY, z, state);
					if ((code & 0x80) != 0) {
						pos.set(blockX, blockY, blockZ);
						chunk.markPosForPostProcessing(pos);
					}
				}
			}
		}
	}

	/** Block state per native palette id. */
	static BlockState[] paletteStates() {
		return palette;
	}

	/**
	 * The block index the ore stage reads sections with: block ids in
	 * registry order, the block of every global block state id, the air
	 * states, and every block tag's members.
	 */
	private static JsonObject blockIndex() {
		JsonObject doc = new JsonObject();
		JsonArray blocks = new JsonArray();
		for (Block block : BuiltInRegistries.BLOCK) {
			blocks.add(BuiltInRegistries.BLOCK.getKey(block).toString());
		}
		JsonArray states = new JsonArray();
		JsonArray air = new JsonArray();
		for (int id = 0; id < Block.BLOCK_STATE_REGISTRY.size(); id++) {
			BlockState state = Block.BLOCK_STATE_REGISTRY.byId(id);
			states.add(state == null ? -1 : BuiltInRegistries.BLOCK.getId(state.getBlock()));
			if (state != null && state.isAir()) {
				air.add(id);
			}
		}
		JsonObject tags = new JsonObject();
		BuiltInRegistries.BLOCK.getTags().forEach(named -> {
			JsonArray members = new JsonArray();
			for (Holder<Block> holder : named) {
				members.add(BuiltInRegistries.BLOCK.getKey(holder.value()).toString());
			}
			tags.add(named.key().location().toString(), members);
		});
		doc.add("blocks", blocks);
		doc.add("states", states);
		doc.add("air", air);
		doc.add("tags", tags);
		return doc;
	}

	private static void add(List<String> kinds, List<String> ids, List<String> bodies, String kind, String id, DataResult<JsonElement> encoded) {
		JsonElement json = encoded.getOrThrow(msg -> new IllegalStateException(kind + " " + id + ": " + msg));
		kinds.add(kind);
		ids.add(id);
		bodies.add(json.toString());
	}

	private static final Strategy<BlockState> BLOCK_STATES = Strategy.createForBlockStates(Block.BLOCK_STATE_REGISTRY);

	/**
	 * Installs a packed chunk (layout: painite_terrain::state26::pack_chunk):
	 * one fresh section per 16 blocks built from its palette and bit storage,
	 * both worldgen heightmaps from raw data, and the post-processing lists.
	 * Returns false, having written nothing, when the blob is malformed.
	 */
	public static boolean writePacked(ChunkAccess chunk, long[] words) {
		BlockState[] states = palette;
		LevelChunkSection[] sections = chunk.getSections();
		int minY = chunk.getMinY();
		if (words.length < 2) {
			return false;
		}
		int sectionCount = (int) words[0];
		int heightmapLongs = (int) words[1];
		if (sectionCount != chunk.getHeight() / 16) {
			return false;
		}
		LevelChunkSection[] fresh = new LevelChunkSection[sectionCount];
		int at = 2;
		for (int s = 0; s < sectionCount; s++) {
			if (at >= words.length) {
				return false;
			}
			long header = words[at++];
			int paletteLen = (int) (header & 0xffff);
			int bits = (int) ((header >> 16) & 0xffff);
			int storageLongs = (int) (header >>> 32);
			int paletteWords = (paletteLen + 3) / 4;
			if (paletteLen == 0 || at + paletteWords + storageLongs > words.length) {
				return false;
			}
			List<BlockState> entries = new ArrayList<>(paletteLen);
			for (int k = 0; k < paletteLen; k++) {
				int id = (int) ((words[at + k / 4] >>> (16 * (k % 4))) & 0xffff);
				if (id >= states.length) {
					return false;
				}
				entries.add(states[id]);
			}
			at += paletteWords;
			Optional<LongStream> storage = storageLongs == 0 ? Optional.empty() : Optional.of(Arrays.stream(words, at, at + storageLongs));
			at += storageLongs;
			DataResult<PalettedContainer<BlockState>> unpacked = PalettedContainer.unpack(BLOCK_STATES,
					new PalettedContainerRO.PackedData<>(entries, storage, bits == 0 ? 0 : bits));
			if (unpacked.isError()) {
				PainiteMod.LOGGER.warn("[painite] rustTerrain: section {} of {} rejected: {}", s, chunk.getPos(), unpacked.error().map(Object::toString).orElse(""));
				return false;
			}
			int index = chunk.getSectionIndex(minY + s * 16);
			fresh[s] = new LevelChunkSection(unpacked.getOrThrow(), sections[index].getBiomes());
		}
		if (at + 2 * heightmapLongs + 1 > words.length) {
			return false;
		}
		long[] surface = Arrays.copyOfRange(words, at, at + heightmapLongs);
		at += heightmapLongs;
		long[] floor = Arrays.copyOfRange(words, at, at + heightmapLongs);
		at += heightmapLongs;
		int postCount = (int) words[at++];
		if (at + postCount != words.length) {
			return false;
		}
		for (int s = 0; s < sectionCount; s++) {
			sections[chunk.getSectionIndex(minY + s * 16)] = fresh[s];
		}
		chunk.getOrCreateHeightmapUnprimed(Heightmap.Types.WORLD_SURFACE_WG).setRawData(chunk, Heightmap.Types.WORLD_SURFACE_WG, surface);
		chunk.getOrCreateHeightmapUnprimed(Heightmap.Types.OCEAN_FLOOR_WG).setRawData(chunk, Heightmap.Types.OCEAN_FLOOR_WG, floor);
		ShortList[] lists = new ShortList[sectionCount];
		for (int i = 0; i < postCount; i++) {
			long w = words[at + i];
			int s = (int) (w >>> 16);
			if (s >= sectionCount) {
				continue;
			}
			if (lists[s] == null) {
				lists[s] = new ShortArrayList();
			}
			lists[s].add((short) (w & 0xffff));
		}
		for (int s = 0; s < sectionCount; s++) {
			if (lists[s] != null) {
				chunk.addPackedPostProcess(lists[s], chunk.getSectionIndex(minY + s * 16));
			}
		}
		return true;
	}

	/**
	 * The pieces a chunk's Beardifier holds, flattened for the native (layout in rust/terrain/src/beard26.rs):
	 * counts, the affected box, then 8 ints per rigid piece and 3 per jigsaw junction. Null when nothing beards
	 * the chunk.
	 */
	public static int[] beardPieces(Beardifier beardifier) {
		if (beardifier == null || beardifier == Beardifier.EMPTY) {
			return null;
		}
		BeardifierAccessor access = (BeardifierAccessor) beardifier;
		BoundingBox affected = access.painite$affectedBox();
		if (affected == null) {
			return null;
		}
		List<Beardifier.Rigid> rigids = access.painite$pieces();
		List<JigsawJunction> junctions = access.painite$junctions();
		int[] out = new int[8 + rigids.size() * 8 + junctions.size() * 3];
		out[0] = rigids.size();
		out[1] = junctions.size();
		out[2] = affected.minX();
		out[3] = affected.minY();
		out[4] = affected.minZ();
		out[5] = affected.maxX();
		out[6] = affected.maxY();
		out[7] = affected.maxZ();
		int at = 8;
		for (Beardifier.Rigid rigid : rigids) {
			BoundingBox box = rigid.box();
			out[at++] = box.minX();
			out[at++] = box.minY();
			out[at++] = box.minZ();
			out[at++] = box.maxX();
			out[at++] = box.maxY();
			out[at++] = box.maxZ();
			out[at++] = rigid.terrainAdjustment().ordinal();
			out[at++] = rigid.groundLevelDelta();
		}
		for (JigsawJunction junction : junctions) {
			out[at++] = junction.getSourceX();
			out[at++] = junction.getSourceGroundY();
			out[at++] = junction.getSourceZ();
		}
		return out;
	}

	/** One line for a far-view record: height range, then top blocks and biomes with column counts. */
	public static String describeLod(ServerLevel level, int[] record) {
		BlockState[] states = palette;
		Holder<Biome>[] holders = biomeHolders;
		int min = Integer.MAX_VALUE;
		int max = Integer.MIN_VALUE;
		Map<String, Integer> tops = new HashMap<>();
		Map<String, Integer> biomes = new HashMap<>();
		for (int c = 0; c < 256; c++) {
			min = Math.min(min, record[c]);
			max = Math.max(max, record[c]);
			int top = record[256 + c];
			int biome = record[512 + c];
			String topName = top >= 0 && top < states.length ? BuiltInRegistries.BLOCK.getKey(states[top].getBlock()).getPath() : "#" + top;
			String biomeName = biome >= 0 && biome < holders.length ? holders[biome].unwrapKey().map(k -> k.identifier().getPath()).orElse("#" + biome) : "#" + biome;
			tops.merge(topName, 1, Integer::sum);
			biomes.merge(biomeName, 1, Integer::sum);
		}
		return "height " + min + ".." + max + ", top " + counts(tops) + ", biome " + counts(biomes);
	}

	private static String counts(Map<String, Integer> counts) {
		return counts.entrySet().stream()
				.sorted((a, b) -> b.getValue() - a.getValue())
				.map(e -> e.getKey() + "=" + e.getValue())
				.reduce((a, b) -> a + " " + b).orElse("none");
	}

	/** Whether far-view records are being kept for the running world. */
	private static byte[] worldId = new byte[16];

	/** Sixteen bytes naming this world to far-view clients, from the seed and the settings id. */
	private static byte[] worldId(long seed, String settings) {
		try {
			java.security.MessageDigest md5 = java.security.MessageDigest.getInstance("MD5");
			md5.update(java.nio.ByteBuffer.allocate(8).putLong(seed).array());
			return md5.digest(("painite:" + settings).getBytes(java.nio.charset.StandardCharsets.UTF_8));
		} catch (java.security.NoSuchAlgorithmException e) {
			throw new IllegalStateException(e);
		}
	}

	public static byte[] worldId() {
		return worldId.clone();
	}

	public static boolean lodActive() {
		return LOD && activeSettings != null;
	}

	/** The block state per native palette id, as the command syntax spells it. */
	public static List<String> paletteStrings() {
		BlockState[] states = palette;
		List<String> out = new ArrayList<>(states.length);
		for (BlockState state : states) {
			out.add(BlockStateParser.serialize(state));
		}
		return out;
	}

	/** The biome id per native biome index. */
	public static List<String> biomeIds() {
		Holder<Biome>[] holders = biomeHolders;
		List<String> out = new ArrayList<>(holders.length);
		for (Holder<Biome> holder : holders) {
			out.add(holder.unwrapKey().map(k -> k.identifier().toString()).orElse(""));
		}
		return out;
	}

	/** Write the changed far-view regions; nothing when no terrain is compiled. */
	public static void flushLod() {
		if (!LOD || activeSettings == null) {
			return;
		}
		int written = PainiteNative.terrainLodFlush();
		if (written < 0) {
			PainiteMod.LOGGER.warn("[painite] far-view records: region write failed");
		} else if (written > 0) {
			PainiteMod.LOGGER.debug("[painite] far-view records: {} regions written", written);
		}
	}

	/** Whether a natively surfaced chunk should also be carved by the native. */
	public static boolean carveNative() {
		return CARVERS && nativeCarvers;
	}

	public static String report() {
		String carve = "";
		if (PainiteNative.AVAILABLE && activeSettings != null) {
			long[] c = PainiteNative.terrainCarveStats();
			if (c != null && c.length >= 6 && c[0] > 0) {
				carve = String.format(Locale.ROOT, "  carvers chunks=%d mask=%.3fms apply=%.3fms carved=%d aquifer=%d topMaterial=%d\n",
						c[0], c[1] / 1e6 / c[0], c[2] / 1e6 / c[0], c[3], c[4], c[5]);
			}
		}
		return "  terrain native=" + NATIVE_FILLS.get() + " vanilla=" + VANILLA_FILLS.get() + " ineligible=" + INELIGIBLE_FILLS.get()
				+ " beard=" + BEARD_FILLS.get()
				+ " surface=" + NATIVE_SURFACES.get() + " surfaceFallback=" + SURFACE_FALLBACKS.get() + " gridFallback=" + GRID_FALLBACKS.get()
				+ " biomes=" + NATIVE_BIOMES.get() + " carved=" + NATIVE_CARVES.get() + "\n"
				+ carve
				+ OreBridge.report();
	}
}
