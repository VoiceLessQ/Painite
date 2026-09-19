package me.apika.painite;

import com.mojang.brigadier.arguments.IntegerArgumentType;
import java.util.HashMap;
import java.util.Map;
import me.apika.painite.lod.FrameProbe;
import me.apika.painite.lod.LodClient;
import me.apika.painite.lod.LodRenderer;
import me.apika.painite.terrain.TerrainBridge;
import net.fabricmc.api.ClientModInitializer;
import net.fabricmc.fabric.api.client.command.v2.ClientCommandRegistrationCallback;
import net.fabricmc.fabric.api.client.command.v2.ClientCommands;
import net.fabricmc.fabric.api.client.command.v2.FabricClientCommandSource;
import net.minecraft.network.chat.Component;

public class PainiteClient implements ClientModInitializer {
	@Override
	public void onInitializeClient() {
		if (TerrainBridge.LOD) {
			LodClient.register();
		}
		ClientCommandRegistrationCallback.EVENT.register((dispatcher, registry) -> dispatcher.register(
				ClientCommands.literal("painitelod")
						.executes(ctx -> summary(ctx.getSource()))
						.then(ClientCommands.literal("frame").executes(ctx -> frame(ctx.getSource())))
						.then(ClientCommands.literal("draw").executes(ctx -> {
							ctx.getSource().sendFeedback(Component.literal(LodRenderer.report()));
							return 1;
						}))
						.then(ClientCommands.argument("chunkX", IntegerArgumentType.integer())
								.then(ClientCommands.argument("chunkZ", IntegerArgumentType.integer())
										.executes(ctx -> chunk(ctx.getSource(),
												IntegerArgumentType.getInteger(ctx, "chunkX"),
												IntegerArgumentType.getInteger(ctx, "chunkZ")))))));
	}

	private static int summary(FabricClientCommandSource source) {
		int held = PainiteNative.AVAILABLE ? PainiteNative.lodClientCount() : 0;
		String text = "[painite] far view: " + (LodClient.ready() ? "receiving" : "no palette") + ", " + LodClient.received() + " received, " + held + " held";
		PainiteMod.LOGGER.info(text);
		source.sendFeedback(Component.literal(text));
		return 1;
	}

	private static int frame(FabricClientCommandSource source) {
		String text = FrameProbe.ENABLED ? FrameProbe.report() : "[painite] frame probe off (-Dpainite.frameProbe=true)";
		source.sendFeedback(Component.literal(text));
		return 1;
	}

	private static int chunk(FabricClientCommandSource source, int chunkX, int chunkZ) {
		int[] record = PainiteNative.AVAILABLE ? PainiteNative.lodClientGet(chunkX, chunkZ) : null;
		String text;
		if (record == null) {
			text = "[painite] far view " + chunkX + "," + chunkZ + ": no record";
		} else {
			int min = Integer.MAX_VALUE;
			int max = Integer.MIN_VALUE;
			Map<String, Integer> tops = new HashMap<>();
			Map<String, Integer> biomes = new HashMap<>();
			for (int c = 0; c < 256; c++) {
				min = Math.min(min, record[c]);
				max = Math.max(max, record[c]);
				tops.merge(LodClient.blockName(record[256 + c]), 1, Integer::sum);
				biomes.merge(LodClient.biomeName(record[512 + c]), 1, Integer::sum);
			}
			text = "[painite] far view " + chunkX + "," + chunkZ + ": height " + min + ".." + max + ", top " + tops + ", biome " + biomes;
		}
		PainiteMod.LOGGER.info(text);
		source.sendFeedback(Component.literal(text));
		return 1;
	}
}
