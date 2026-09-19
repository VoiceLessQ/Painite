package me.apika.painite.probe;

import java.io.IOException;
import java.io.Writer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Random;
import net.minecraft.core.Holder;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.ResourceKey;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.world.level.chunk.ChunkGenerator;
import net.minecraft.world.level.block.Blocks;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.levelgen.Aquifer;
import net.minecraft.world.level.levelgen.NoiseBasedChunkGenerator;
import net.minecraft.world.level.levelgen.NoiseChunk;
import net.minecraft.world.level.levelgen.NoiseGeneratorSettings;
import net.minecraft.world.level.levelgen.blending.Blender;
import net.minecraft.world.level.levelgen.densityfunction.DensitySampler;
import net.minecraft.world.level.levelgen.densityfunction.DensityVolume;
import net.minecraft.world.level.levelgen.densityfunction.ScopedDensityBuffer;
import net.minecraft.world.level.levelgen.NoiseRouter;
import net.minecraft.world.level.levelgen.RandomState;
import net.minecraft.world.level.levelgen.densityfunction.DensityFunction;
import net.minecraft.world.level.levelgen.synth.NormalNoise;

/**
 * Samples vanilla's compiled density functions at random block positions
 * and writes them as float bits, so the Rust terrain crate can check its
 * own evaluation against the game bit for bit.
 */
public final class DensityOracle {
	public static final Path OUTPUT = Path.of("painite_density_oracle.txt");
	public static final Path CHUNK_OUTPUT = Path.of("painite_density_chunk.txt");
	private static final int XZ_RANGE = 4096;

	private DensityOracle() {}

	/** Returns the number of samples written. */
	public static int write(ServerLevel level, int count, long positionSeed) throws IOException {
		ChunkGenerator generator = level.getChunkSource().getGenerator();
		if (!(generator instanceof NoiseBasedChunkGenerator noiseGenerator)) {
			throw new IOException("generator is " + generator.getClass().getSimpleName() + ", not noise based");
		}
		RandomState randomState = level.getChunkSource().randomState();
		NoiseRouter router = noiseGenerator.generatorSettings().value().noiseRouter();
		int minY = noiseGenerator.generatorSettings().value().noiseSettings().minY();
		int height = noiseGenerator.generatorSettings().value().noiseSettings().height();
		String settingsKey = noiseGenerator.generatorSettings().unwrapKey()
				.map(k -> k.identifier().toString()).orElse("inline");

		List<ResourceKey<NormalNoise>> noiseKeys = level.registryAccess().lookupOrThrow(Registries.NOISE)
				.listElements().map(Holder.Reference::key).sorted((a, b) -> a.identifier().compareTo(b.identifier())).toList();

		Random random = new Random(positionSeed);
		int written = 0;
		try (Writer out = Files.newBufferedWriter(OUTPUT, StandardCharsets.UTF_8)) {
			out.write("# seed " + level.getSeed() + " settings " + settingsKey
					+ " min_y " + minY + " height " + height + " position_seed " + positionSeed + "\n");
			for (int i = 0; i < count; i++) {
				int x = random.nextInt(2 * XZ_RANGE) - XZ_RANGE;
				int z = random.nextInt(2 * XZ_RANGE) - XZ_RANGE;
				int y = minY + random.nextInt(height);
				out.write(line("temperature", router.temperature(), randomState, x, y, z));
				out.write(line("vegetation", router.vegetation(), randomState, x, y, z));
				out.write(line("continents", router.continents(), randomState, x, y, z));
				out.write(line("erosion", router.erosion(), randomState, x, y, z));
				out.write(line("depth", router.depth(), randomState, x, y, z));
				out.write(line("ridges", router.ridges(), randomState, x, y, z));
				out.write(line("chunk_surface_level", router.chunkSurfaceLevel(), randomState, x, y, z));
				out.write(line("final_density", router.finalDensity(), randomState, x, y, z));
				for (ResourceKey<NormalNoise> key : noiseKeys) {
					float value = randomState.getOrCreateNoise(key).get(x, y, z);
					out.write("noise:" + key.identifier() + " " + x + " " + y + " " + z + " "
							+ Integer.toHexString(Float.floatToRawIntBits(value)) + " " + value + "\n");
				}
				written++;
			}
		}
		return written;
	}

	/**
	 * Writes the final_density buffer of one chunk column exactly as the
	 * TERRAIN stage samples it (no structure beards, no blending), one
	 * value per line in buffer index order. Returns the value count.
	 */
	public static int writeChunk(ServerLevel level, int chunkX, int chunkZ) throws IOException {
		ChunkGenerator generator = level.getChunkSource().getGenerator();
		if (!(generator instanceof NoiseBasedChunkGenerator noiseGenerator)) {
			throw new IOException("generator is " + generator.getClass().getSimpleName() + ", not noise based");
		}
		RandomState randomState = level.getChunkSource().randomState();
		NoiseGeneratorSettings settings = noiseGenerator.generatorSettings().value();
		int minY = settings.noiseSettings().minY();
		int height = settings.noiseSettings().height();
		DensityVolume volume = new DensityVolume(16, height, 16, chunkX * 16, minY, chunkZ * 16);
		// Same rule as NoiseBasedChunkGenerator.createFluidPicker (private there).
		Aquifer.FluidStatus lavaStatus = new Aquifer.FluidStatus(-54, Blocks.LAVA.defaultBlockState());
		Aquifer.FluidStatus seaStatus = new Aquifer.FluidStatus(settings.seaLevel(), settings.defaultFluid());
		int lavaBelow = Math.min(-54, settings.seaLevel());
		Aquifer.FluidPicker picker = (x, y, z) -> y < lavaBelow ? lavaStatus : seaStatus;
		int written = 0;
		try (NoiseChunk noiseChunk = new NoiseChunk(randomState, null, settings, picker, Blender.empty(), volume);
				Writer out = Files.newBufferedWriter(CHUNK_OUTPUT, StandardCharsets.UTF_8)) {
			DensitySampler.Bound finalDensity = noiseChunk.cachingSamplers().get(settings.noiseRouter().finalDensity());
			out.write("# seed " + level.getSeed() + " chunk " + chunkX + " " + chunkZ + " min_y " + minY + " height " + height
					+ " sea_level " + settings.seaLevel() + " substance 0=default 1=air 2=water 3=lava 4=other\n");
			Aquifer aquifer = noiseChunk.aquifer();
			try (ScopedDensityBuffer buffer = finalDensity.sampleVolume(volume)) {
				// Same visiting order as doFill: z, x, then y from the top down.
				String[] lines = new String[volume.size()];
				for (int z = 0; z < volume.sizeZ(); z++) {
					for (int x = 0; x < volume.sizeX(); x++) {
						for (int y = volume.sizeY() - 1; y >= 0; y--) {
							int index = volume.indexUnchecked(x, y, z);
							float density = buffer.get(index);
							BlockState state = aquifer.computeSubstance(volume.blockX(x), volume.blockY(y), volume.blockZ(z), density);
							int substance = state == null ? 0 : state.isAir() ? 1 : state.is(Blocks.WATER) ? 2 : state.is(Blocks.LAVA) ? 3 : 4;
							if (state != null && aquifer.shouldScheduleFluidUpdate() && !state.getFluidState().isEmpty()) {
								substance += 16;
							}
							lines[index] = Integer.toHexString(Float.floatToRawIntBits(density)) + " " + substance;
						}
					}
				}
				for (String line : lines) {
					out.write(line);
					out.write('\n');
					written++;
				}
			}
			dumpAquifer(out, aquifer);
			dumpAquiferProbes(out, aquifer, noiseChunk, randomState, settings);
		}
		return written;
	}

	/** Reflective dump of the aquifer caches so the Rust port can be checked cell by cell. */
	private static void dumpAquifer(Writer out, Aquifer aquifer) throws IOException {
		try {
			Class<?> c = aquifer.getClass();
			int minGridX = (int) field(c, "minGridX").get(aquifer);
			int minGridY = (int) field(c, "minGridY").get(aquifer);
			int minGridZ = (int) field(c, "minGridZ").get(aquifer);
			int gridSizeX = (int) field(c, "gridSizeX").get(aquifer);
			int gridSizeZ = (int) field(c, "gridSizeZ").get(aquifer);
			int skip = (int) field(c, "skipSamplingAboveY").get(aquifer);
			long[] locations = (long[]) field(c, "aquiferLocationCache").get(aquifer);
			Object[] statuses = (Object[]) field(c, "aquiferCache").get(aquifer);
			out.write("aq grid " + minGridX + " " + minGridY + " " + minGridZ + " " + gridSizeX + " " + gridSizeZ + " skip " + skip + "\n");
			for (int i = 0; i < locations.length; i++) {
				if (locations[i] == Long.MAX_VALUE) {
					continue;
				}
				long l = locations[i];
				String status = "none";
				if (statuses[i] != null) {
					Aquifer.FluidStatus fs = (Aquifer.FluidStatus) statuses[i];
					int type = fs.fluidType().isAir() ? 1 : fs.fluidType().is(Blocks.WATER) ? 2 : fs.fluidType().is(Blocks.LAVA) ? 3 : 4;
					status = fs.fluidLevel() + " " + type;
				}
				out.write("aq cell " + i + " " + net.minecraft.core.BlockPos.getX(l) + " " + net.minecraft.core.BlockPos.getY(l) + " "
						+ net.minecraft.core.BlockPos.getZ(l) + " " + status + "\n");
			}
			@SuppressWarnings("unchecked")
			it.unimi.dsi.fastutil.longs.Long2IntMap surfaces = (it.unimi.dsi.fastutil.longs.Long2IntMap) field(c, "surfaceLevelCache").get(aquifer);
			for (it.unimi.dsi.fastutil.longs.Long2IntMap.Entry e : surfaces.long2IntEntrySet()) {
				long k = e.getLongKey();
				out.write("aq surface " + net.minecraft.world.level.ChunkPos.getX(k) + " " + net.minecraft.world.level.ChunkPos.getZ(k) + " " + e.getIntValue() + "\n");
			}
		} catch (ReflectiveOperationException e) {
			out.write("# aquifer dump failed: " + e + "\n");
		}
	}

	/** For every computed aquifer cell: the config noises as the aquifer saw them (cached context) and uncached. */
	private static void dumpAquiferProbes(Writer out, Aquifer aquifer, NoiseChunk noiseChunk, RandomState randomState,
			NoiseGeneratorSettings settings) throws IOException {
		try {
			Class<?> c = aquifer.getClass();
			long[] locations = (long[]) field(c, "aquiferLocationCache").get(aquifer);
			Object[] statuses = (Object[]) field(c, "aquiferCache").get(aquifer);
			Aquifer.Config config = settings.aquifers().orElseThrow();
			for (int i = 0; i < locations.length; i++) {
				if (statuses[i] == null) {
					continue;
				}
				int x = net.minecraft.core.BlockPos.getX(locations[i]);
				int y = net.minecraft.core.BlockPos.getY(locations[i]);
				int z = net.minecraft.core.BlockPos.getZ(locations[i]);
				float floodCached = noiseChunk.cachingSamplers().sampleValue(config.fluidLevelFloodednessNoise(), x, y, z);
				float floodRaw = randomState.sampleBlockValueUncached(config.fluidLevelFloodednessNoise(), x, y, z);
				float exclCached = noiseChunk.cachingSamplers().sampleValue(config.exclusion(), x, y, z);
				float exclRaw = randomState.sampleBlockValueUncached(config.exclusion(), x, y, z);
				int cx = Math.floorDiv(x, 16), cy = Math.floorDiv(y, 40), cz = Math.floorDiv(z, 16);
				float spreadCached = noiseChunk.cachingSamplers().sampleValue(config.fluidLevelSpreadNoise(), cx, cy, cz);
				float spreadRaw = randomState.sampleBlockValueUncached(config.fluidLevelSpreadNoise(), cx, cy, cz);
				out.write("aq probe " + i + " " + Integer.toHexString(Float.floatToRawIntBits(floodCached)) + " " + Integer.toHexString(Float.floatToRawIntBits(floodRaw))
						+ " " + Integer.toHexString(Float.floatToRawIntBits(exclCached)) + " " + Integer.toHexString(Float.floatToRawIntBits(exclRaw))
						+ " " + Integer.toHexString(Float.floatToRawIntBits(spreadCached)) + " " + Integer.toHexString(Float.floatToRawIntBits(spreadRaw)) + "\n");
			}
		} catch (ReflectiveOperationException e) {
			out.write("# aquifer probe dump failed: " + e + "\n");
		}
	}

	private static java.lang.reflect.Field field(Class<?> c, String name) throws NoSuchFieldException {
		java.lang.reflect.Field f = c.getDeclaredField(name);
		f.setAccessible(true);
		return f;
	}

	private static String line(String key, DensityFunction function, RandomState state, int x, int y, int z) {
		float value = state.sampleBlockValueUncached(function, x, y, z);
		return key + " " + x + " " + y + " " + z + " " + Integer.toHexString(Float.floatToRawIntBits(value)) + " " + value + "\n";
	}
}
