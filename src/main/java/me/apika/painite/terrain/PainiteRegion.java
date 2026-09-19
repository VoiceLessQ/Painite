package me.apika.painite.terrain;

import net.minecraft.world.level.levelgen.Heightmap;

/** Implemented on WorldGenRegion by mixin: a heightmap read that keeps the last chunk it touched. */
public interface PainiteRegion {
	/** Same value as getHeight; repeat reads of one chunk skip the region lookup. */
	int painite$heightCached(Heightmap.Types type, int x, int z);
}
