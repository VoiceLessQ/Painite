package me.apika.painite.lod;

import me.apika.painite.PainiteNative;
import me.apika.painite.terrain.TerrainBridge;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerChunkEvents;
import net.minecraft.core.BlockPos;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.Level;
import net.minecraft.tags.FluidTags;
import net.minecraft.world.level.block.Blocks;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.level.chunk.LevelChunk;
import net.minecraft.world.level.levelgen.Heightmap;

/** Refreshes a chunk's far-view record from the finished chunk, so trees, snow and placed blocks show. */
public final class LodRefresh {
	private static long refreshed;

	private LodRefresh() {}

	public static void register() {
		ServerChunkEvents.CHUNK_GENERATE.register((level, chunk) -> refresh(level.dimension() == Level.OVERWORLD, chunk));
		ServerChunkEvents.CHUNK_LOAD.register((level, chunk, generated) -> refresh(level.dimension() == Level.OVERWORLD, chunk));
	}

	/** Server thread. Generated chunks always refresh; loaded ones only while their record is still the generator's. */
	private static void refresh(boolean overworld, LevelChunk chunk) {
		if (!overworld || !TerrainBridge.lodActive()) {
			return;
		}
		ChunkPos pos = chunk.getPos();
		if (PainiteNative.terrainLodStage(pos.x(), pos.z()) == 1) {
			return;
		}
		int[] heights = new int[256];
		int[] tops = new int[256];
		int[] depths = new int[256];
		int[] floors = new int[256];
		int water = LodPalette.idFor(Blocks.WATER.defaultBlockState());
		int minY = chunk.getMinY();
		BlockPos.MutableBlockPos at = new BlockPos.MutableBlockPos();
		for (int z = 0; z < 16; z++) {
			for (int x = 0; x < 16; x++) {
				// getHeight is the top block; the record holds the first free y above it, as the generator's does.
				int h = chunk.getHeight(Heightmap.Types.WORLD_SURFACE, x, z) + 1;
				int c = x + z * 16;
				heights[c] = h;
				if (h > minY) {
					at.set(pos.getMinBlockX() + x, h - 1, pos.getMinBlockZ() + z);
					BlockState top = chunk.getBlockState(at);
					tops[c] = LodPalette.idFor(top);
					floors[c] = tops[c];
					if (top.getFluidState().is(FluidTags.WATER)) {
						// Kelp, seagrass and waterlogged blocks read as the water they stand in.
						tops[c] = water;
						// Down through the water to the floor block, as the far view shades it.
						int depth = 1;
						at.setY(h - 2);
						while (at.getY() >= minY && chunk.getBlockState(at).getFluidState().is(FluidTags.WATER)) {
							depth++;
							at.setY(at.getY() - 1);
						}
						depths[c] = Math.min(depth, 255);
						floors[c] = at.getY() >= minY ? LodPalette.idFor(chunk.getBlockState(at)) : tops[c];
					}
				}
			}
		}
		if (PainiteNative.terrainLodRefresh(pos.x(), pos.z(), heights, tops, depths, floors) == 1) {
			refreshed++;
		}
	}

	public static long refreshed() {
		return refreshed;
	}
}
