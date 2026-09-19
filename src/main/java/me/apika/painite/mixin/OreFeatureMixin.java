package me.apika.painite.mixin;

import com.llamalad7.mixinextras.injector.wrapmethod.WrapMethod;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import com.llamalad7.mixinextras.injector.wrapoperation.WrapOperation;
import me.apika.painite.probe.BusyProbe;
import me.apika.painite.terrain.PainiteRegion;
import net.minecraft.core.BlockPos;
import net.minecraft.util.RandomSource;
import net.minecraft.world.level.WorldGenLevel;
import net.minecraft.world.level.chunk.ChunkGenerator;
import net.minecraft.world.level.levelgen.Heightmap;
import net.minecraft.world.level.levelgen.feature.OreFeature;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Unique;
import org.spongepowered.asm.mixin.injection.At;

/**
 * Ore placement starts with a heightmap probe over a square around the
 * origin, up to 81 reads through the region's chunk lookup; 63% of the
 * calls in a burst exhaust the square and place nothing. Consecutive
 * probes read the same chunk almost every time, so the region keeps
 * the chunk it last read and answers repeat probes from it
 * (WorldGenRegionMixin). Same object the region would return, same
 * value. -Dpainite.oreProbe=false restores the plain lookup.
 */
@Mixin(OreFeature.class)
public abstract class OreFeatureMixin {
	@Unique
	private static final boolean PROBE_CACHE = Boolean.parseBoolean(System.getProperty("painite.oreProbe", "true"));

	@WrapMethod(method = "place")
	private boolean painite$timePlace(WorldGenLevel level, ChunkGenerator generator, RandomSource random, BlockPos origin, Operation<Boolean> original) {
		BusyProbe p = BusyProbe.of("features.ore.place");
		long t0 = p.start();
		try {
			return original.call(level, generator, random, origin);
		} finally {
			p.stop(t0);
		}
	}

	@WrapOperation(method = "place", at = @At(value = "INVOKE", target = "Lnet/minecraft/world/level/WorldGenLevel;getHeight(Lnet/minecraft/world/level/levelgen/Heightmap$Types;II)I"))
	private int painite$cachedHeight(WorldGenLevel level, Heightmap.Types type, int x, int z, Operation<Integer> original) {
		if (PROBE_CACHE && level instanceof PainiteRegion region) {
			return region.painite$heightCached(type, x, z);
		}
		return original.call(level, type, x, z);
	}
}
