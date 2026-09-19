package me.apika.painite.lod;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import me.apika.painite.PainiteMod;
import me.apika.painite.terrain.TerrainBridge;
import net.minecraft.commands.arguments.blocks.BlockStateParser;
import net.minecraft.world.level.block.state.BlockState;

/** Palette ids for far-view records: the generator's palette first, then blocks seen on finished chunks, kept in a file. */
public final class LodPalette {
	/** First id of the extra palette; the generator's palette stays below it. */
	public static final int EXTRA_BASE = 4096;
	private static final Map<String, Integer> IDS = new HashMap<>();
	private static final List<String> EXTRA = new ArrayList<>();
	private static Path file;
	private static boolean loaded;

	private LodPalette() {}

	/** Server thread: read the extra names kept next to the region files. */
	public static synchronized void load(Path lodDir) {
		IDS.clear();
		EXTRA.clear();
		List<String> names = TerrainBridge.paletteStrings();
		for (int i = 0; i < names.size(); i++) {
			IDS.put(names.get(i), i);
		}
		file = lodDir.resolve("extra_palette.txt");
		if (Files.exists(file)) {
			try {
				for (String line : Files.readAllLines(file, StandardCharsets.UTF_8)) {
					if (!line.isEmpty()) {
						IDS.putIfAbsent(line, EXTRA_BASE + EXTRA.size());
						EXTRA.add(line);
					}
				}
			} catch (IOException e) {
				PainiteMod.LOGGER.warn("[painite] far view: cannot read {}: {}", file, e.getMessage());
			}
		}
		loaded = true;
	}

	public static synchronized void unload() {
		IDS.clear();
		EXTRA.clear();
		file = null;
		loaded = false;
	}

	/** The extra names in id order. */
	public static synchronized List<String> extra() {
		return List.copyOf(EXTRA);
	}

	/** Server thread: the id of a block state, adding it to the extra palette (and its file) the first time. */
	public static synchronized int idFor(BlockState state) {
		if (!loaded) {
			return 0;
		}
		String name = BlockStateParser.serialize(state);
		Integer id = IDS.get(name);
		if (id != null) {
			return id;
		}
		int next = EXTRA_BASE + EXTRA.size();
		IDS.put(name, next);
		EXTRA.add(name);
		if (file != null) {
			try {
				Files.writeString(file, name + "\n", StandardCharsets.UTF_8, StandardOpenOption.CREATE, StandardOpenOption.APPEND);
			} catch (IOException e) {
				PainiteMod.LOGGER.warn("[painite] far view: cannot append to {}: {}", file, e.getMessage());
			}
		}
		LodSender.broadcastExtra(next - EXTRA_BASE, List.of(name));
		return next;
	}
}
