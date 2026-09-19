package me.apika.painite.mixin;

import me.apika.painite.terrain.PainiteRegion;
import net.minecraft.core.SectionPos;
import net.minecraft.server.level.WorldGenRegion;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.levelgen.Heightmap;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Unique;

/**
 * A region hands out the same chunk object for the same coordinates for
 * its whole life, so a heightmap read can reuse the chunk from the
 * previous read instead of the distance check and cache lookup. The
 * region is used by one thread per FEATURES call.
 */
@Mixin(WorldGenRegion.class)
public abstract class WorldGenRegionMixin implements PainiteRegion {
	@Unique
	private ChunkAccess painite$lastChunk;

	@Override
	public int painite$heightCached(Heightmap.Types type, int x, int z) {
		int chunkX = SectionPos.blockToSectionCoord(x);
		int chunkZ = SectionPos.blockToSectionCoord(z);
		ChunkAccess chunk = this.painite$lastChunk;
		if (chunk == null || chunk.getPos().x() != chunkX || chunk.getPos().z() != chunkZ) {
			WorldGenRegion self = (WorldGenRegion) (Object) this;
			int height = self.getHeight(type, x, z);
			this.painite$lastChunk = self.getChunk(chunkX, chunkZ);
			return height;
		}
		return chunk.getHeight(type, x & 15, z & 15) + 1;
	}
}
