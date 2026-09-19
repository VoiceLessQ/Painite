package me.apika.painite.command;

import com.mojang.brigadier.CommandDispatcher;
import com.mojang.brigadier.arguments.IntegerArgumentType;
import com.mojang.brigadier.arguments.LongArgumentType;
import com.google.gson.JsonElement;
import com.mojang.serialization.JsonOps;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import me.apika.painite.PainiteMod;
import me.apika.painite.PainiteNative;
import me.apika.painite.lod.LodSender;
import me.apika.painite.probe.BusyProbe;
import me.apika.painite.probe.DensityOracle;
import me.apika.painite.probe.DispatcherProbe;
import me.apika.painite.probe.FeatureProbe;
import me.apika.painite.probe.LightProbe;
import me.apika.painite.probe.StageProbe;
import me.apika.painite.probe.StepProbe;
import me.apika.painite.probe.SurfaceOracle;
import me.apika.painite.terrain.TerrainBridge;
import net.minecraft.commands.CommandSourceStack;
import net.minecraft.commands.Commands;
import net.minecraft.network.chat.Component;
import net.minecraft.resources.RegistryOps;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.world.level.levelgen.NoiseBasedChunkGenerator;

public final class PainiteCommand {
	private PainiteCommand() {}

	public static void register(CommandDispatcher<CommandSourceStack> dispatcher) {
		dispatcher.register(Commands.literal("painite")
				.requires(Commands.hasPermission(Commands.LEVEL_GAMEMASTERS))
				.then(Commands.literal("probe")
						.then(Commands.literal("report").executes(ctx -> {
							String text = (StageProbe.report() + DispatcherProbe.report() + StepProbe.report() + LightProbe.report() + BusyProbe.report() + FeatureProbe.report() + TerrainBridge.report())
									+ (PainiteNative.AVAILABLE
											? "  native active=" + PainiteNative.active() + " waiting=" + PainiteNative.waiting()
											: "  native unavailable");
							PainiteMod.LOGGER.info(text);
							ctx.getSource().sendSuccess(() -> Component.literal(text), false);
							return 1;
						}))
						.then(Commands.literal("reset").executes(ctx -> {
							StageProbe.reset();
							DispatcherProbe.reset();
							StepProbe.reset();
							LightProbe.reset();
							BusyProbe.reset();
							FeatureProbe.reset();
							ctx.getSource().sendSuccess(() -> Component.literal("[painite] probe reset"), false);
							return 1;
						})))
				.then(Commands.literal("lod")
						.then(Commands.literal("send").executes(ctx -> {
							String text = LodSender.report();
							PainiteMod.LOGGER.info(text);
							ctx.getSource().sendSuccess(() -> Component.literal(text), false);
							return 1;
						}))
						.then(Commands.argument("chunkX", IntegerArgumentType.integer())
								.then(Commands.argument("chunkZ", IntegerArgumentType.integer())
										.executes(ctx -> lodChunk(ctx.getSource(),
												IntegerArgumentType.getInteger(ctx, "chunkX"),
												IntegerArgumentType.getInteger(ctx, "chunkZ"))))))
				.then(Commands.literal("density")
						.then(Commands.literal("probe")
								.then(Commands.argument("count", IntegerArgumentType.integer(1, 1_000_000))
										.executes(ctx -> densityProbe(ctx.getSource(), IntegerArgumentType.getInteger(ctx, "count"), 0L))
										.then(Commands.argument("positionSeed", LongArgumentType.longArg())
												.executes(ctx -> densityProbe(ctx.getSource(),
														IntegerArgumentType.getInteger(ctx, "count"),
														LongArgumentType.getLong(ctx, "positionSeed"))))))
						.then(Commands.literal("chunk")
								.then(Commands.argument("chunkX", IntegerArgumentType.integer())
										.then(Commands.argument("chunkZ", IntegerArgumentType.integer())
												.executes(ctx -> densityChunk(ctx.getSource(),
														IntegerArgumentType.getInteger(ctx, "chunkX"),
														IntegerArgumentType.getInteger(ctx, "chunkZ"))))))
						.then(Commands.literal("biomes").executes(ctx -> biomeParameters(ctx.getSource())))
						.then(Commands.literal("surface")
								.then(Commands.argument("chunkX", IntegerArgumentType.integer())
										.then(Commands.argument("chunkZ", IntegerArgumentType.integer())
												.executes(ctx -> surfaceChunk(ctx.getSource(),
														IntegerArgumentType.getInteger(ctx, "chunkX"),
														IntegerArgumentType.getInteger(ctx, "chunkZ"))))))));
	}

	private static int biomeParameters(CommandSourceStack source) {
		try {
			ServerLevel level = source.getLevel();
			if (!(level.getChunkSource().getGenerator() instanceof NoiseBasedChunkGenerator generator)) {
				source.sendFailure(Component.literal("[painite] generator is not noise based"));
				return 0;
			}
			RegistryOps<JsonElement> ops = RegistryOps.create(JsonOps.INSTANCE, level.registryAccess());
			String json = TerrainBridge.biomeParameters(level, generator, ops);
			if (json == null) {
				source.sendFailure(Component.literal("[painite] biome source is not multi-noise"));
				return 0;
			}
			Path out = Path.of("painite_biome_parameters.json");
			Files.writeString(out, json);
			String text = "[painite] biome parameters -> " + out.toAbsolutePath();
			source.sendSuccess(() -> Component.literal(text), false);
			return 1;
		} catch (IOException | RuntimeException e) {
			source.sendFailure(Component.literal("[painite] biome parameters failed: " + e.getMessage()));
			return 0;
		}
	}

	/** Summarise a chunk's far-view record: height range, top blocks and biomes by column count. */
	private static int lodChunk(CommandSourceStack source, int chunkX, int chunkZ) {
		int[] record = PainiteNative.AVAILABLE ? PainiteNative.terrainLod(chunkX, chunkZ) : null;
		if (record == null || record.length != 768) {
			source.sendFailure(Component.literal("[painite] no far-view record for chunk " + chunkX + "," + chunkZ));
			return 0;
		}
		String text = "[painite] lod " + chunkX + "," + chunkZ + ": " + TerrainBridge.describeLod(source.getLevel(), record);
		PainiteMod.LOGGER.info(text);
		source.sendSuccess(() -> Component.literal(text), false);
		return 1;
	}

	private static int surfaceChunk(CommandSourceStack source, int chunkX, int chunkZ) {
		String text = "[painite] " + SurfaceOracle.generate(source.getLevel(), chunkX, chunkZ);
		PainiteMod.LOGGER.info(text);
		source.sendSuccess(() -> Component.literal(text), false);
		return 1;
	}

	private static int densityChunk(CommandSourceStack source, int chunkX, int chunkZ) {
		try {
			int written = DensityOracle.writeChunk(source.getLevel(), chunkX, chunkZ);
			String text = "[painite] density chunk: " + written + " values -> " + DensityOracle.CHUNK_OUTPUT.toAbsolutePath();
			PainiteMod.LOGGER.info(text);
			source.sendSuccess(() -> Component.literal(text), false);
			return 1;
		} catch (IOException e) {
			source.sendFailure(Component.literal("[painite] density chunk failed: " + e.getMessage()));
			return 0;
		}
	}

	private static int densityProbe(CommandSourceStack source, int count, long positionSeed) {
		try {
			int written = DensityOracle.write(source.getLevel(), count, positionSeed);
			String text = "[painite] density oracle: " + written + " positions -> " + DensityOracle.OUTPUT.toAbsolutePath();
			PainiteMod.LOGGER.info(text);
			source.sendSuccess(() -> Component.literal(text), false);
			return written;
		} catch (IOException e) {
			source.sendFailure(Component.literal("[painite] density oracle failed: " + e.getMessage()));
			return 0;
		}
	}
}
