package me.apika.painite.mixin;

import com.llamalad7.mixinextras.injector.wrapmethod.WrapMethod;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import me.apika.painite.PainiteNative;
import me.apika.painite.probe.BusyProbe;
import me.apika.painite.terrain.PainiteNoiseChunk;
import me.apika.painite.probe.SurfaceOracle;
import me.apika.painite.terrain.TerrainBridge;
import java.util.Set;
import net.minecraft.core.Holder;
import net.minecraft.world.level.biome.Biome;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.levelgen.NoiseBasedChunkGenerator;
import net.minecraft.world.level.levelgen.NoiseChunk;
import net.minecraft.server.level.WorldGenRegion;
import net.minecraft.world.level.biome.BiomeManager;
import net.minecraft.world.level.levelgen.RandomState;
import net.minecraft.world.level.levelgen.blending.Blender;
import net.minecraft.world.level.levelgen.densityfunction.DensityVolume;
import net.minecraft.world.level.levelgen.material.rule.MaterialRule;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Shadow;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/** Replaces doFill with the native fill when the world and chunk qualify. Busy timers around every part. */
@Mixin(NoiseBasedChunkGenerator.class)
public abstract class NoiseBasedChunkGeneratorMixin {
	@Shadow
	private void doFill(NoiseChunk noiseChunk, ChunkAccess chunk) {
		throw new AssertionError();
	}

	@WrapMethod(method = "createNoiseChunk")
	private NoiseChunk painite$timeNoiseChunk(ChunkAccess chunk, net.minecraft.world.level.StructureManager structureManager, Blender blender,
			RandomState randomState, net.minecraft.world.level.levelgen.NoiseSettings settings, Operation<NoiseChunk> original) {
		BusyProbe p = BusyProbe.of("terrain.noisechunk");
		long t0 = p.start();
		try {
			return original.call(chunk, structureManager, blender, randomState, settings);
		} finally {
			p.stop(t0);
		}
	}

	@WrapMethod(method = "doFill")
	private void painite$timeFill(NoiseChunk noiseChunk, ChunkAccess chunk, Operation<Void> original) {
		BusyProbe p = BusyProbe.of("terrain.fill.total");
		long t0 = p.start();
		try {
			original.call(noiseChunk, chunk);
		} finally {
			p.stop(t0);
		}
	}

	@Inject(method = "doFill", at = @At("HEAD"), cancellable = true)
	private void painite$nativeFill(NoiseChunk noiseChunk, ChunkAccess chunk, CallbackInfo ci) {
		if (!TerrainBridge.ENABLED || !TerrainBridge.serves((NoiseBasedChunkGenerator) (Object) this)) {
			TerrainBridge.VANILLA_FILLS.incrementAndGet();
			return;
		}
		if (!((PainiteNoiseChunk) noiseChunk).painite$eligible()) {
			TerrainBridge.INELIGIBLE_FILLS.incrementAndGet();
			return;
		}
		int[] beard = ((PainiteNoiseChunk) noiseChunk).painite$beard();
		BusyProbe p = BusyProbe.of(beard == null ? "terrain.fill.native" : "terrain.fill.beard");
		long t0 = p.start();
		int result = PainiteNative.terrainFill(chunk.getPos().x(), chunk.getPos().z(), beard);
		p.stop(t0);
		if (result != 1) {
			TerrainBridge.VANILLA_FILLS.incrementAndGet();
			return;
		}
		if (beard != null) {
			TerrainBridge.BEARD_FILLS.incrementAndGet();
		}
		// The blocks stay in the native until buildSurface writes fill and surface together.
		((PainiteNoiseChunk) noiseChunk).painite$setNativeFilled(true);
		TerrainBridge.NATIVE_FILLS.incrementAndGet();
		ci.cancel();
	}

	@WrapMethod(method = "buildSurface")
	private void painite$timeSurface(ChunkAccess chunk, NoiseChunk noiseChunk, RandomState randomState, BiomeManager biomeManager,
			Set<Holder<Biome>> possibleBiomes, MaterialRule materialRule, Operation<Void> original) {
		BusyProbe p = BusyProbe.of("terrain.surface.total");
		long t0 = p.start();
		try {
			original.call(chunk, noiseChunk, randomState, biomeManager, possibleBiomes, materialRule);
		} finally {
			p.stop(t0);
		}
	}

	@Inject(method = "buildSurface", at = @At("HEAD"), cancellable = true)
	private void painite$nativeSurface(ChunkAccess chunk, NoiseChunk noiseChunk, RandomState randomState, BiomeManager biomeManager,
			Set<Holder<Biome>> possibleBiomes, MaterialRule materialRule, CallbackInfo ci) {
		if (!((PainiteNoiseChunk) noiseChunk).painite$nativeFilled()) {
			return;
		}
		int chunkX = chunk.getPos().x();
		int chunkZ = chunk.getPos().z();
		DensityVolume volume = noiseChunk.volume();
		// Fresh chunks only: an upgrading chunk keeps vanilla's protected top rows in the mask.
		boolean carve = TerrainBridge.carveNative() && !chunk.isUpgrading();
		BusyProbe nativeProbe = BusyProbe.of("terrain.surface.native");
		long t1 = nativeProbe.start();
		long[] packed = PainiteNative.terrainSurfacePacked(chunkX, chunkZ, null, carve);
		nativeProbe.stop(t1);
		if (packed == null) {
			// The native has no biome output for a neighbour (vanilla biomes, or evicted): read the grid here.
			TerrainBridge.GRID_FALLBACKS.incrementAndGet();
			BusyProbe quartsProbe = BusyProbe.of("terrain.surface.quarts");
			long t0 = quartsProbe.start();
			int[] quarts = TerrainBridge.quartGrid(biomeManager, chunk);
			quartsProbe.stop(t0);
			if (quarts != null) {
				long t2 = nativeProbe.start();
				packed = PainiteNative.terrainSurfacePacked(chunkX, chunkZ, quarts, carve);
				nativeProbe.stop(t2);
			}
		}
		if (packed != null) {
			BusyProbe writeProbe = BusyProbe.of("terrain.surface.write");
			long t2 = writeProbe.start();
			boolean written = TerrainBridge.writePacked(chunk, packed);
			writeProbe.stop(t2);
			if (written) {
				TerrainBridge.NATIVE_SURFACES.incrementAndGet();
				if (carve) {
					((PainiteNoiseChunk) noiseChunk).painite$setNativeCarved(true);
					TerrainBridge.NATIVE_CARVES.incrementAndGet();
				}
				ci.cancel();
				return;
			}
		}
		// Surface declined: write the fill alone and let vanilla build the surface.
		TerrainBridge.SURFACE_FALLBACKS.incrementAndGet();
		byte[] fill = packed == null ? PainiteNative.terrainTake(chunkX, chunkZ) : null;
		if (fill != null && fill.length == volume.size()) {
			TerrainBridge.writeBlocks(chunk, volume, fill);
			return;
		}
		// The native no longer holds a usable fill (a rejected write consumed it): run vanilla's.
		TerrainBridge.LOST_FILLS.incrementAndGet();
		((PainiteNoiseChunk) noiseChunk).painite$decline();
		this.doFill(noiseChunk, chunk);
	}

	@WrapMethod(method = "generateCarvers")
	private void painite$timeCarvers(ChunkAccess chunk, Blender blender, NoiseChunk noiseChunk, RandomState randomState,
			BiomeManager biomeManager, WorldGenRegion carverBiomeRegion, MaterialRule materialRule, Operation<Void> original) {
		BusyProbe p = BusyProbe.of("terrain.carvers");
		long t0 = p.start();
		try {
			original.call(chunk, blender, noiseChunk, randomState, biomeManager, carverBiomeRegion, materialRule);
		} finally {
			p.stop(t0);
		}
	}

	@WrapMethod(method = "applyCarvingMask")
	private void painite$timeCarverApply(ChunkAccess chunk, net.minecraft.world.level.chunk.CarvingMask mask, RandomState randomState,
			MaterialRule materialRule, net.minecraft.world.level.levelgen.WorldGenerationContext context, NoiseChunk noiseChunk,
			java.util.function.Function<net.minecraft.core.BlockPos, Holder<Biome>> biomeGetter,
			net.minecraft.world.level.chunk.CarvingMask.Filter filter, Operation<Void> original) {
		BusyProbe p = BusyProbe.of("terrain.carvers.apply");
		long t0 = p.start();
		try {
			original.call(chunk, mask, randomState, materialRule, context, noiseChunk, biomeGetter, filter);
		} finally {
			p.stop(t0);
		}
	}

	@Inject(method = "generateCarvers", at = @At("HEAD"), cancellable = true)
	private void painite$surfaceOracle(ChunkAccess chunk, Blender blender, NoiseChunk noiseChunk, RandomState randomState,
			BiomeManager biomeManager, WorldGenRegion carverBiomeRegion, MaterialRule materialRule, CallbackInfo ci) {
		PainiteNoiseChunk painiteChunk = (PainiteNoiseChunk) noiseChunk;
		SurfaceOracle.maybeDump(chunk, biomeManager, painiteChunk.painite$eligible(), painiteChunk.painite$beard());
		if (painiteChunk.painite$nativeCarved()) {
			// The native surface pass already ran the carvers on this chunk.
			ci.cancel();
		}
	}

	@Inject(method = "generateCarvers", at = @At("RETURN"))
	private void painite$carvedOracle(ChunkAccess chunk, Blender blender, NoiseChunk noiseChunk, RandomState randomState,
			BiomeManager biomeManager, WorldGenRegion carverBiomeRegion, MaterialRule materialRule, CallbackInfo ci) {
		PainiteNoiseChunk painiteChunk = (PainiteNoiseChunk) noiseChunk;
		SurfaceOracle.maybeDumpCarved(chunk, biomeManager, painiteChunk.painite$eligible(), painiteChunk.painite$beard());
	}
}
