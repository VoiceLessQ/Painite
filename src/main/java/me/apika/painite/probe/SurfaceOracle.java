package me.apika.painite.probe;

import com.google.gson.JsonElement;
import com.mojang.serialization.JsonOps;
import it.unimi.dsi.fastutil.shorts.ShortList;
import java.io.IOException;
import java.io.Writer;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicReference;
import me.apika.painite.PainiteMod;
import net.minecraft.core.Holder;
import net.minecraft.core.QuartPos;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.biome.Biome;
import net.minecraft.world.level.biome.BiomeManager;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.chunk.status.ChunkStatus;
import net.minecraft.world.level.levelgen.Heightmap;
import net.minecraft.world.level.levelgen.NoiseBasedChunkGenerator;

/**
 * Dumps one chunk as it stands between buildSurface and the carvers:
 * every block state, both worldgen heightmaps, the post-processing set,
 * the biome quart grid the surface pass read and the structure pieces
 * that bearded the fill. The Rust surface port is checked against this file.
 */
public final class SurfaceOracle {
	public static final Path OUTPUT = Path.of("painite_surface_chunk.txt");
	/** The same chunk after generateCarvers, for the carver port. */
	public static final Path CARVED_OUTPUT = Path.of("painite_carved_chunk.txt");
	private static final AtomicReference<ChunkPos> TARGET = new AtomicReference<>();
	private static volatile String lastResult;
	private static volatile long seed;

	private SurfaceOracle() {}

	/** Arms the dump for one chunk, then forces its generation on the calling thread. */
	public static String generate(ServerLevel level, int chunkX, int chunkZ) {
		if (!(level.getChunkSource().getGenerator() instanceof NoiseBasedChunkGenerator)) {
			return "generator is not noise based";
		}
		ChunkPos pos = new ChunkPos(chunkX, chunkZ);
		if (level.getChunkSource().getChunk(chunkX, chunkZ, ChunkStatus.EMPTY, false) != null
				&& level.getChunkSource().getChunkNow(chunkX, chunkZ) != null) {
			return "chunk " + pos + " is already loaded; pick an ungenerated one";
		}
		lastResult = null;
		seed = level.getSeed();
		TARGET.set(pos);
		try {
			level.getChunk(chunkX, chunkZ, ChunkStatus.FULL, true);
		} finally {
			TARGET.set(null);
		}
		String result = lastResult;
		return result == null ? "chunk " + pos + " was already generated, nothing dumped" : result;
	}

	/** Called from the generator mixin at the start of generateCarvers. */
	public static void maybeDump(ChunkAccess chunk, BiomeManager biomeManager, boolean eligible, int[] beard) {
		ChunkPos target = TARGET.get();
		if (target == null || !target.equals(chunk.getPos())) {
			return;
		}
		try {
			int written = dump(chunk, biomeManager, eligible, beard, OUTPUT);
			lastResult = "surface oracle: " + written + " blocks -> " + OUTPUT.toAbsolutePath();
		} catch (IOException e) {
			lastResult = "surface oracle failed: " + e.getMessage();
			PainiteMod.LOGGER.warn("[painite] surface oracle failed", e);
		}
	}

	/** Called from the generator mixin at the end of generateCarvers: the surface dump's layout, after the carvers. */
	public static void maybeDumpCarved(ChunkAccess chunk, BiomeManager biomeManager, boolean eligible, int[] beard) {
		ChunkPos target = TARGET.get();
		if (target == null || !target.equals(chunk.getPos())) {
			return;
		}
		try {
			int written = dump(chunk, biomeManager, eligible, beard, CARVED_OUTPUT);
			lastResult = lastResult + "; carved oracle: " + written + " blocks -> " + CARVED_OUTPUT.toAbsolutePath();
		} catch (IOException e) {
			lastResult = lastResult + "; carved oracle failed: " + e.getMessage();
			PainiteMod.LOGGER.warn("[painite] carved oracle failed", e);
		}
	}

	private static int dump(ChunkAccess chunk, BiomeManager biomeManager, boolean eligible, int[] beard, Path output) throws IOException {
		ChunkPos pos = chunk.getPos();
		int minY = chunk.getMinY();
		int height = chunk.getHeight();
		List<String> palette = new ArrayList<>();
		Map<BlockState, Integer> paletteIndex = new HashMap<>();
		int[] blocks = new int[16 * 16 * height];
		boolean[] post = new boolean[blocks.length];
		ShortList[] postLists = chunk.getPostProcessing();
		for (int sectionIndex = 0; sectionIndex < postLists.length; sectionIndex++) {
			ShortList list = postLists[sectionIndex];
			if (list == null) {
				continue;
			}
			int sectionMinY = chunk.getSectionYFromSectionIndex(sectionIndex) * 16;
			for (int i = 0; i < list.size(); i++) {
				int packed = list.getShort(i);
				// ProtoChunk.packOffsetCoordinates: x | y << 4 | z << 8.
				int x = packed & 15;
				int y = sectionMinY + ((packed >> 4) & 15) - minY;
				int z = (packed >> 8) & 15;
				if (y >= 0 && y < height) {
					post[y + (x + z * 16) * height] = true;
				}
			}
		}
		net.minecraft.core.BlockPos.MutableBlockPos blockPos = new net.minecraft.core.BlockPos.MutableBlockPos();
		for (int z = 0; z < 16; z++) {
			for (int x = 0; x < 16; x++) {
				for (int y = 0; y < height; y++) {
					BlockState state = chunk.getBlockState(blockPos.set(pos.getMinBlockX() + x, minY + y, pos.getMinBlockZ() + z));
					Integer index = paletteIndex.get(state);
					if (index == null) {
						index = palette.size();
						palette.add(encode(state));
						paletteIndex.put(state, index);
					}
					blocks[y + (x + z * 16) * height] = index;
				}
			}
		}
		int quartMinY = QuartPos.fromBlock(minY);
		int quartCount = QuartPos.fromBlock(height);
		int quartX0 = pos.x() * 4 - 1;
		int quartZ0 = pos.z() * 4 - 1;
		List<String> biomeNames = new ArrayList<>();
		Map<String, Integer> biomeIndex = new HashMap<>();
		int[] quarts = new int[6 * 6 * quartCount];
		for (int qz = 0; qz < 6; qz++) {
			for (int qx = 0; qx < 6; qx++) {
				for (int qy = 0; qy < quartCount; qy++) {
					Holder<Biome> biome = biomeManager.getNoiseBiomeAtQuart(quartX0 + qx, quartMinY + qy, quartZ0 + qz);
					String name = biome.unwrapKey().map(k -> k.identifier().toString()).orElse("?");
					Integer index = biomeIndex.get(name);
					if (index == null) {
						index = biomeNames.size();
						biomeNames.add(name);
						biomeIndex.put(name, index);
					}
					quarts[qy + (qx + qz * 6) * quartCount] = index;
				}
			}
		}
		Heightmap surface = chunk.getOrCreateHeightmapUnprimed(Heightmap.Types.WORLD_SURFACE_WG);
		Heightmap floor = chunk.getOrCreateHeightmapUnprimed(Heightmap.Types.OCEAN_FLOOR_WG);
		try (Writer out = Files.newBufferedWriter(output, StandardCharsets.UTF_8)) {
			out.write("# seed " + seed + " chunk " + pos.x() + " " + pos.z() + " eligible " + (eligible ? 1 : 0) + " min_y " + minY + " height " + height + " palette " + palette.size()
					+ " biomes " + biomeNames.size() + " quart_min_y " + quartMinY + " quart_count " + quartCount + "\n");
			for (String state : palette) {
				out.write("palette " + state + "\n");
			}
			for (String name : biomeNames) {
				out.write("biome " + name + "\n");
			}
			if (beard != null) {
				StringBuilder pieces = new StringBuilder("beard");
				for (int v : beard) {
					pieces.append(' ').append(v);
				}
				out.write(pieces.append('\n').toString());
			}
			StringBuilder line = new StringBuilder();
			for (int i = 0; i < quarts.length; i++) {
				line.append(quarts[i]).append(i + 1 < quarts.length ? ' ' : '\n');
			}
			out.write("quarts " + line);
			for (int z = 0; z < 16; z++) {
				for (int x = 0; x < 16; x++) {
					out.write("heights " + x + " " + z + " " + surface.getFirstAvailable(x, z) + " " + floor.getFirstAvailable(x, z) + "\n");
				}
			}
			for (int i = 0; i < blocks.length; i++) {
				out.write(Integer.toString(post[i] ? blocks[i] | 0x8000 : blocks[i]));
				out.write('\n');
			}
		}
		return blocks.length;
	}

	private static String encode(BlockState state) {
		JsonElement json = BlockState.CODEC.encodeStart(JsonOps.INSTANCE, state).getOrThrow();
		return json.toString();
	}
}
