package me.apika.painite.lod;

import java.io.File;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import net.fabricmc.fabric.api.client.event.lifecycle.v1.ClientTickEvents;
import me.apika.painite.PainiteMod;
import me.apika.painite.PainiteNative;
import net.fabricmc.fabric.api.client.networking.v1.ClientPlayConnectionEvents;
import net.fabricmc.fabric.api.client.networking.v1.ClientPlayNetworking;
import net.minecraft.client.Minecraft;
import net.minecraft.client.player.LocalPlayer;
import net.minecraft.commands.arguments.blocks.BlockStateParser;
import net.minecraft.core.BlockPos;
import net.minecraft.core.registries.BuiltInRegistries;
import net.minecraft.core.registries.Registries;
import net.minecraft.resources.Identifier;
import net.minecraft.resources.ResourceKey;
import net.minecraft.world.level.biome.Biome;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.block.state.BlockState;
import com.mojang.blaze3d.platform.NativeImage;
import me.apika.painite.mixin.client.SpriteContentsAccessor;
import net.minecraft.client.color.block.BlockTintSource;
import net.minecraft.client.renderer.texture.SpriteContents;
import net.minecraft.client.renderer.texture.TextureAtlasSprite;
import net.minecraft.util.ARGB;
import net.minecraft.client.renderer.block.dispatch.BlockStateModel;
import net.minecraft.client.renderer.block.dispatch.BlockStateModelPart;
import net.minecraft.client.resources.model.geometry.BakedQuad;
import net.minecraft.core.Direction;
import net.minecraft.util.RandomSource;

/** The receiving end: records go into the native client store, the palette stays here for naming. */
public final class LodClient {
	private static volatile List<String> blocks = List.of();
	private static volatile List<String> biomes = List.of();
	private static final List<String> extra = new java.util.ArrayList<>();
	private static volatile boolean ready;
	private static long received;
	private static boolean versionWarned;
	private static volatile int farRadius = 32;
	/** Regions whose have bitmap went to the server; forgotten past twice the far radius, as the server does. */
	private static final Set<Long> reported = new HashSet<>();
	/** Regions brought in from disk for drawing; forgotten past twice the draw radius so they come back later. */
	private static final Set<Long> warmed = new HashSet<>();
	private static int lastChunkX = Integer.MIN_VALUE;
	private static int lastChunkZ = Integer.MIN_VALUE;
	private static int tick;
	private static boolean onDisk;
	private static final int FLUSH_EVERY_TICKS = 100;
	private static final java.util.concurrent.atomic.AtomicLong fromDisk = new java.util.concurrent.atomic.AtomicLong();

	private LodClient() {}

	public static void register() {
		ClientPlayNetworking.registerGlobalReceiver(LodPayloads.Palette.TYPE, (payload, context) -> onPalette(payload));
		ClientPlayNetworking.registerGlobalReceiver(LodPayloads.Batch.TYPE, (payload, context) -> onBatch(payload, context.player()));
		ClientPlayNetworking.registerGlobalReceiver(LodPayloads.PaletteExtra.TYPE, (payload, context) -> onExtra(payload));
		ClientPlayConnectionEvents.DISCONNECT.register((listener, client) -> client.execute(LodClient::clear));
		ClientTickEvents.END_CLIENT_TICK.register(client -> {
			if (ready && client.player != null) {
				tickWindow(client.player);
			}
		});
	}

	private static void onPalette(LodPayloads.Palette payload) {
		clear();
		if (payload.version() != LodPayloads.PROTOCOL) {
			if (!versionWarned) {
				versionWarned = true;
				PainiteMod.LOGGER.warn("[painite] far view: server speaks version {}, this build {}; ignoring its records", payload.version(), LodPayloads.PROTOCOL);
			}
			return;
		}
		blocks = List.copyOf(payload.blocks());
		biomes = List.copyOf(payload.biomes());
		extra.clear();
		extra.addAll(payload.extra());
		farRadius = payload.farRadius();
		ready = PainiteNative.AVAILABLE;
		if (ready) {
			PainiteNative.lodClientColours(colours());
			waterColours();
			onDisk = payload.worldId().length == 16 && PainiteNative.lodClientOpen(storeDir(payload.worldId()), payload.worldId()) == 1;
			if (!onDisk) {
				PainiteMod.LOGGER.warn("[painite] far view: records stay in memory only (no store directory)");
			}
		}
		PainiteMod.LOGGER.info("[painite] far view: palette of {} blocks and {} biomes, far radius {}", blocks.size(), biomes.size(), farRadius);
	}

	private static void onBatch(LodPayloads.Batch payload, LocalPlayer player) {
		if (!ready) {
			return;
		}
		ChunkPos pos = player.chunkPosition();
		int taken = PainiteNative.lodClientPut(payload.entries(), pos.x(), pos.z());
		if (taken < 0) {
			PainiteMod.LOGGER.warn("[painite] far view: torn batch of {} bytes dropped", payload.entries().length);
			return;
		}
		boolean first = received == 0;
		received += taken;
		if (first || received % 256 < taken) {
			PainiteMod.LOGGER.info("[painite] far view: {} records received, {} from disk, {} held", received, fromDisk.get(), PainiteNative.lodClientCount());
		}
	}

	/** Extra palette names appended by the server while we are connected. */
	private static void onExtra(LodPayloads.PaletteExtra payload) {
		if (!ready || payload.start() != extra.size()) {
			return;
		}
		extra.addAll(payload.names());
		PainiteNative.lodClientColours(colours());
	}

	/** Water's surface colour per biome of the palette: the texture's mean times the biome's water colour, so the far
	 *  sea changes colour where the biomes do; the water id is the palette entry a water column carries as its top. */
	private static void waterColours() {
		int water = -1;
		for (int i = 0; i < blocks.size(); i++) {
			if (blocks.get(i).startsWith("minecraft:water[")) {
				water = i;
				break;
			}
		}
		Minecraft minecraft = Minecraft.getInstance();
		if (water < 0 || minecraft.level == null) {
			PainiteNative.lodClientWaterColours(-1, new int[0]);
			return;
		}
		int[] out = new int[biomes.size()];
		int texture;
		try {
			texture = textureColour(BlockStateParser.parseForBlock(BuiltInRegistries.BLOCK, blocks.get(water), false).blockState(), false);
		} catch (Exception e) {
			texture = 0;
		}
		if (texture == 0) {
			PainiteNative.lodClientWaterColours(-1, new int[0]);
			return;
		}
		var registry = minecraft.level.registryAccess().lookupOrThrow(Registries.BIOME);
		for (int i = 0; i < out.length; i++) {
			int colour = 0x3f76e4;
			try {
				var biome = registry.get(ResourceKey.create(Registries.BIOME, Identifier.parse(biomes.get(i))));
				if (biome.isPresent()) {
					colour = biome.get().value().getWaterColor();
				}
			} catch (Exception ignored) {
				// An unparsable biome name keeps the default water colour.
			}
			out[i] = ARGB.multiply(texture, 0xff000000 | colour);
		}
		PainiteNative.lodClientWaterColours(water, out);
	}

	/** Map colour per palette id as ARGB, generator palette then the extra one from 4096; unparsable entries draw magenta. */
	private static int[] colours() {
		int[] out = new int[LodPalette.EXTRA_BASE + extra.size()];
		for (int i = 0; i < blocks.size(); i++) {
			out[i] = colour(blocks.get(i));
		}
		for (int i = 0; i < extra.size(); i++) {
			out[LodPalette.EXTRA_BASE + i] = colour(extra.get(i));
		}
		StringBuilder sample = new StringBuilder();
		for (int i = 0; i < blocks.size(); i++) {
			String name = blocks.get(i);
			if (name.startsWith("minecraft:water") || name.startsWith("minecraft:grass_block") || name.startsWith("minecraft:sand")) {
				sample.append(' ').append(name).append(String.format("=#%06x", out[i] & 0xffffff));
			}
		}
		PainiteMod.LOGGER.info("[painite] far view colours:{}", sample);
		return out;
	}

	/** The block's texture averaged and tinted, as Distant Horizons colours its LODs; the map colour when there is no texture. */
	private static int colour(String name) {
		try {
			BlockState state = BlockStateParser.parseForBlock(BuiltInRegistries.BLOCK, name, false).blockState();
			int textured = textureColour(state, true);
			return textured != 0 ? textured : 0xff000000 | state.getMapColor(Minecraft.getInstance().level, BlockPos.ZERO).col;
		} catch (Exception e) {
			return 0xffff00ff;
		}
	}

	/** Mean of the opaque pixels of the block's top face texture (its particle sprite when the model has no top),
	 *  times that face's tint for the biome the player stands in, alpha the mean over every pixel; 0 without a usable sprite. */
	private static int textureColour(BlockState state, boolean tinted) {
		Minecraft minecraft = Minecraft.getInstance();
		BlockPos at = minecraft.player != null ? minecraft.player.blockPosition() : BlockPos.ZERO;
		BlockStateModel model = minecraft.getModelManager().getBlockStateModelSet().get(state);
		List<BlockStateModelPart> parts = new java.util.ArrayList<>();
		model.collectParts(RandomSource.create(42), parts);
		TextureAtlasSprite sprite = null;
		int tint = -1;
		for (BlockStateModelPart part : parts) {
			List<BakedQuad> quads = part.getQuads(Direction.UP);
			if (!quads.isEmpty()) {
				sprite = quads.get(0).materialInfo().sprite();
				int index = quads.get(0).materialInfo().tintIndex();
				List<BlockTintSource> tints = minecraft.getBlockColors().getTintSources(state);
				if (index >= 0 && !tints.isEmpty()) {
					tint = tints.get(Math.min(index, tints.size() - 1)).colorInWorld(state, minecraft.level, at);
				}
				break;
			}
		}
		if (sprite == null) {
			// Fluids and other model-less blocks: the particle sprite, tinted as a terrain particle would be (water gets its biome colour).
			sprite = model.particleMaterial().sprite();
			List<BlockTintSource> tints = minecraft.getBlockColors().getTintSources(state);
			if (!tints.isEmpty()) {
				tint = tints.get(0).colorAsTerrainParticle(state, minecraft.level, at);
			}
		}
		if (sprite == null || sprite.contents().name().getPath().equals("missingno")) {
			return 0;
		}
		SpriteContents contents = sprite.contents();
		NativeImage image = ((SpriteContentsAccessor) contents).painite$originalImage();
		int width = Math.min(contents.width(), image.getWidth());
		int height = Math.min(contents.height(), image.getHeight());
		long r = 0;
		long g = 0;
		long b = 0;
		long a = 0;
		int n = 0;
		// The first animation frame is the top-left width x height of the image.
		for (int y = 0; y < height; y++) {
			for (int x = 0; x < width; x++) {
				int argb = image.getPixel(x, y);
				a += ARGB.alpha(argb);
				if (ARGB.alpha(argb) < 128) {
					continue;
				}
				r += ARGB.red(argb);
				g += ARGB.green(argb);
				b += ARGB.blue(argb);
				n++;
			}
		}
		if (n == 0) {
			return 0;
		}
		// The mean alpha rides along: it is how much of the floor shows through water.
		int colour = ARGB.color((int) (a / ((long) width * height)), (int) (r / n), (int) (g / n), (int) (b / n));
		return tinted ? ARGB.multiply(colour, 0xff000000 | tint) : colour;
	}

	/** painite/lod/[server]/[world id] under the game directory. */
	private static String storeDir(byte[] worldId) {
		Minecraft minecraft = Minecraft.getInstance();
		String server = minecraft.getCurrentServer() == null ? "singleplayer" : minecraft.getCurrentServer().ip.replaceAll("[^A-Za-z0-9._-]", "_");
		StringBuilder hex = new StringBuilder();
		for (byte b : worldId) {
			hex.append(String.format("%02x", b));
		}
		return new File(new File(new File(minecraft.gameDirectory, "painite"), "lod"), server + File.separator + hex).getAbsolutePath();
	}

	/** Each tick: regions entering the far window come in from disk and their have bitmaps go to the server. */
	private static void tickWindow(LocalPlayer player) {
		ChunkPos pos = player.chunkPosition();
		if (++tick % FLUSH_EVERY_TICKS == 0 && onDisk) {
			PainiteNative.lodClientFlush();
		}
		if (pos.x() == lastChunkX && pos.z() == lastChunkZ) {
			return;
		}
		lastChunkX = pos.x();
		lastChunkZ = pos.z();
		int keep = farRadius * 2;
		reported.removeIf(key -> beyond(key, pos, keep));
		int drawRadius = LodRenderer.VIEW;
		warmed.removeIf(key -> beyond(key, pos, drawRadius * 2));
		List<long[]> toWarm = new java.util.ArrayList<>();
		for (int rz = (pos.z() - drawRadius) >> 5; rz <= (pos.z() + drawRadius) >> 5; rz++) {
			for (int rx = (pos.x() - drawRadius) >> 5; rx <= (pos.x() + drawRadius) >> 5; rx++) {
				if (warmed.add(((long) rx << 32) ^ (rz & 0xffffffffL))) {
					toWarm.add(new long[] {rx, rz});
				}
			}
		}
		if (!toWarm.isEmpty()) {
			// Region files are read off this thread; the renderer picks the records up through the region generation.
			LodRenderer.offThread(() -> {
				for (long[] region : toWarm) {
					int loaded = PainiteNative.lodClientWarm((int) region[0], (int) region[1]);
					if (loaded > 0 && fromDisk.getAndAdd(loaded) == 0) {
						PainiteMod.LOGGER.info("[painite] far view: {} records from disk in the first region, more follow", loaded);
					}
				}
			});
		}
		boolean canSend = ClientPlayNetworking.canSend(LodPayloads.Have.TYPE);
		List<LodPayloads.Have> report = new java.util.ArrayList<>();
		for (int rz = (pos.z() - farRadius) >> 5; rz <= (pos.z() + farRadius) >> 5; rz++) {
			for (int rx = (pos.x() - farRadius) >> 5; rx <= (pos.x() + farRadius) >> 5; rx++) {
				long key = ((long) rx << 32) ^ (rz & 0xffffffffL);
				if (!reported.add(key)) {
					continue;
				}
				byte[] have = PainiteNative.lodClientHave(rx, rz);
				if (have != null && have.length == LodPayloads.HAVE_BYTES) {
					report.add(new LodPayloads.Have(rx, rz, have, false));
				}
			}
		}
		// Every region in the window goes out, empty ones too; the last one tells the server to start.
		if (canSend && !report.isEmpty()) {
			for (int i = 0; i < report.size(); i++) {
				LodPayloads.Have have = report.get(i);
				ClientPlayNetworking.send(i == report.size() - 1 ? new LodPayloads.Have(have.rx(), have.rz(), have.bitmap(), true) : have);
			}
		}
	}

	private static boolean beyond(long key, ChunkPos pos, int chunks) {
		int rx = (int) (key >> 32);
		int rz = (int) key;
		return Math.abs((rx << 5) + 16 - pos.x()) > chunks + 32 || Math.abs((rz << 5) + 16 - pos.z()) > chunks + 32;
	}

	public static int farRadius() {
		return farRadius;
	}

	private static void clear() {
		LodRenderer.clear();
		reported.clear();
		warmed.clear();
		lastChunkX = Integer.MIN_VALUE;
		lastChunkZ = Integer.MIN_VALUE;
		onDisk = false;
		if (received > 0 || fromDisk.get() > 0) {
			PainiteMod.LOGGER.info("[painite] far view: leaving with {} records received, {} from disk, {} held", received, fromDisk.get(), PainiteNative.lodClientCount());
		}
		fromDisk.set(0);
		if (PainiteNative.AVAILABLE) {
			PainiteNative.lodClientClear();
		}
		blocks = List.of();
		biomes = List.of();
		extra.clear();
		ready = false;
		received = 0;
	}

	public static boolean ready() {
		return ready;
	}

	public static long received() {
		return received;
	}

	public static String blockName(int id) {
		List<String> names = blocks;
		if (id >= LodPalette.EXTRA_BASE && id - LodPalette.EXTRA_BASE < extra.size()) {
			return extra.get(id - LodPalette.EXTRA_BASE);
		}
		return id >= 0 && id < names.size() ? names.get(id) : "#" + id;
	}

	public static String biomeName(int index) {
		List<String> names = biomes;
		return index >= 0 && index < names.size() ? names.get(index) : "#" + index;
	}
}
