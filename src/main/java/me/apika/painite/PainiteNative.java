package me.apika.painite;

import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.StandardCopyOption;
import java.util.Locale;

/** Loads the bundled native and exposes the scheduler calls. */
public final class PainiteNative {
	private static final String NATIVE_WINDOWS = "/assets/painite/natives/windows/painite.dll";
	private static final String NATIVE_LINUX = "/assets/painite/natives/linux/libpainite.so";
	private static final String NATIVE_LINUX_AARCH64 = "/assets/painite/natives/linux-aarch64/libpainite.so";
	private static final String NATIVE_MACOS = "/assets/painite/natives/macos/libpainite.dylib";

	public static final boolean AVAILABLE;

	static {
		AVAILABLE = load();
	}

	private PainiteNative() {}

	private static boolean load() {
		String osName = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
		String osArch = System.getProperty("os.arch", "").toLowerCase(Locale.ROOT);
		String resourcePath;
		String suffix;
		if (osName.contains("win")) {
			resourcePath = NATIVE_WINDOWS;
			suffix = ".dll";
		} else if (osName.contains("linux")) {
			resourcePath = (osArch.contains("aarch64") || osArch.contains("arm64")) ? NATIVE_LINUX_AARCH64 : NATIVE_LINUX;
			suffix = ".so";
		} else if (osName.contains("mac") || osName.contains("darwin")) {
			resourcePath = NATIVE_MACOS;
			suffix = ".dylib";
		} else {
			PainiteMod.LOGGER.warn("[painite] unsupported OS \"{}\", running without native", osName);
			return false;
		}
		try (InputStream in = PainiteNative.class.getResourceAsStream(resourcePath)) {
			if (in == null) {
				PainiteMod.LOGGER.error("[painite] native not bundled at {}, running without native", resourcePath);
				return false;
			}
			File tmp = File.createTempFile("painite_", suffix);
			tmp.deleteOnExit();
			Files.copy(in, tmp.toPath(), StandardCopyOption.REPLACE_EXISTING);
			System.load(tmp.getAbsolutePath());
			PainiteMod.LOGGER.info("[painite] loaded native from {}", tmp.getAbsolutePath());
			return true;
		} catch (UnsatisfiedLinkError | IOException e) {
			PainiteMod.LOGGER.error("[painite] native failed to load, running without native: {}", e.getMessage());
			return false;
		}
	}

	/** 1 on first init, 0 if already initialised. */
	public static native int init(int maxFeatures, int maxInflight);

	/** Blocks until the job may run. Returns the ticket seq, or -1 when ungated. */
	/** Returns seq << 1 | granted; a negative value means no scheduler. */
	public static native long submit(int stage, int cx, int cz, int level);

	/** Frees the zone; returns the seqs of parked jobs that may run now (never null unless out of memory). */
	public static native long[] release(int stage, int cx, int cz, long seq);

	/** Compile a world's terrain from datapack JSON; 1 = native available, 0 = vanilla path, -1 = bad call. */
	public static native int terrainInit(long seed, long biomeZoomSeed, String settingsId, String[] kinds, String[] ids, String[] bodies);

	/** Canonical block state JSON per palette id, or null when no terrain is compiled. */
	public static native String[] terrainPalette();

	/** Per-palette flags (1 air, 2 fluid, 4 blocks motion in the heightmap); 1 = accepted. */
	public static native int terrainPaletteFlags(byte[] flags);

	/**
	 * Surface pass on a pending fill; palette id | 0x80 (post-process) per block. {@code quarts} null uses the
	 * biome output the native kept for the chunk and its neighbours. Null when nothing is pending or no grid is
	 * usable; the fill then stays pending for a retry with a grid or terrainTake.
	 */
	public static native byte[] terrainSurface(int chunkX, int chunkZ, int[] quarts);

	/** A pending fill without its surface pass, same encoding as terrainSurface, or null. */
	public static native byte[] terrainTake(int chunkX, int chunkZ);

	/**
	 * terrainSurface as packed sections, heightmap raw data and post-processing positions, or null. With
	 * {@code carve} the carvers run on the surfaced chunk first; null when the native cannot carve (the fill stays pending).
	 */
	public static native long[] terrainSurfacePacked(int chunkX, int chunkZ, int[] quarts, boolean carve);

	/** 1 when the compiled terrain can run the carvers (every carver the biomes name loaded), else 0. */
	public static native int terrainCarvers();

	/** Carve totals since load: chunks, mask ns, apply ns, blocks carved, aquifer calls, top material calls. */
	public static native long[] terrainCarveStats();

	/** terrainTake in the packed layout, or null. */
	public static native long[] terrainTakePacked(int chunkX, int chunkZ);

	/** Biome index per quart, y + (x + z * 4) * (height / 4), or null when the native has no biome stage. */
	public static native int[] terrainBiomes(int chunkX, int chunkZ);

	public static native void terrainClear();

	/** Placed ore feature ids the native serves, in index order, or null without an ore stage. */
	public static native String[] terrainOrePlaced();

	/** Keep an ore batch; returns the section range lo | hi << 16 the apply call needs, or -1. Null quarts as in terrainSurface. */
	public static native int terrainOrePlan(int chunkX, int chunkZ, long[] seeds, int[] placed, int[] quarts);

	/**
	 * Run a planned batch over packed sections (six ints of meta per section:
	 * slot | section << 4, palette length or 0 for global, bits, palette
	 * offset, storage offset, storage length). Two ints per write:
	 * slot | section << 4 | packed << 16 and the palette id. Null on a bad batch.
	 */
	public static native int[] terrainOreApply(int chunkX, int chunkZ, long[] heights, int[] meta, int[] palettes, long[] storage);

	/**
	 * Fill one chunk and keep it pending for terrainSurface or terrainTake. {@code beard} is the structure
	 * pieces near the chunk as built by TerrainBridge.beardPieces, or null. 1 = pending, 0 = no terrain
	 * compiled, -1 = malformed pieces (the chunk is left to vanilla).
	 */
	public static native int terrainFill(int chunkX, int chunkZ, int[] beard);

	/** Directory the far-view column records persist to; 1 = set, 0 = no terrain, -1 = cannot create. */
	public static native int terrainLodDir(String dir);

	/** A chunk's far-view record: 256 heights, 256 top palette ids, 256 biome indices; null when never surfaced. */
	public static native int[] terrainLod(int chunkX, int chunkZ);

	/** Write changed far-view regions; count written, or -1 on a write error. */
	public static native int terrainLodFlush();

	/** Next far-view batch for a player at chunk (cx, cz): entries of chunk x, chunk z, record; null when nothing is due. */
	public static native byte[] terrainLodBatch(long player, int cx, int cz, int far, int near, int max);

	/** Drop a player's far-view send state. */
	public static native void terrainLodForget(long player);

	/** Keep a received far-view batch on the client; count taken, -1 for a torn batch. */
	public static native int lodClientPut(byte[] batch, int cx, int cz);

	/** A received far-view record, same layout as terrainLod, or null. */
	public static native int[] lodClientGet(int chunkX, int chunkZ);

	/** Chunk records the client holds, full and coarse. */
	public static native int lodClientCount();

	/** Drop every received far-view record. */
	public static native void lodClientClear();

	/** Persist the client store under a directory, files tagged with the 16-byte world id; 1 on success. */
	public static native int lodClientOpen(String dir, byte[] worldId);

	/** Load a region's records from the client's files into memory once; how many came in. */
	public static native int lodClientWarm(int rx, int rz);

	/** One bit per chunk of a region the client holds, 128 bytes. */
	public static native byte[] lodClientHave(int rx, int rz);

	/** Write the client's dirty region files; how many, -1 on an I/O error. */
	public static native int lodClientFlush();

	/** Replace a chunk's heights and top blocks from the finished chunk; 1 when kept. */
	public static native int terrainLodRefresh(int cx, int cz, int[] heights, int[] tops, int[] depths, int[] floors);

	/** The stage of a chunk's record: 0 generator surface, 1 finished chunk, -1 none. */
	public static native int terrainLodStage(int cx, int cz);

	/** A player reports the chunks of a region it already holds; they are not sent again. */
	public static native void terrainLodHave(long player, int rx, int rz, byte[] bitmap);

	/** Colour per palette id as ARGB, in palette order, for the far-view mesh. */
	public static native void lodClientColours(int[] argb);

	/** Water's palette id and its surface colour per biome index (ARGB, alpha the texture's), for the far mesh. */
	public static native void lodClientWaterColours(int water, int[] argb);

	/** Records that have landed in a 32x32 chunk region since the store was cleared. */
	public static native long lodClientRegionGeneration(int rx, int rz);

	/** Position-colour vertices (16 bytes each) of a region, each 8x8-chunk block at the scale for its distance from
	 *  the player's chunk, relative to the region's block corner, written into the direct buffer when they fit;
	 *  chunkVertices (1024 ints, x + z * 32) gets each chunk's count. Low 32 bits: byte length (0 for nothing to draw; past the capacity
	 *  means nothing was written), then the lowest and highest y as 16-bit values. */
	public static native long lodClientMeshRegion(int rx, int rz, int px, int pz, java.nio.ByteBuffer dst, int[] chunkVertices);

	public static native int active();

	public static native int waiting();
}
