package me.apika.painite.mixin;

import com.llamalad7.mixinextras.injector.wrapmethod.WrapMethod;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import me.apika.painite.probe.BusyProbe;
import me.apika.painite.probe.FeatureProbe;
import me.apika.painite.terrain.OreBridge;
import me.apika.painite.terrain.TerrainBridge;
import net.minecraft.core.BlockPos;
import net.minecraft.util.RandomSource;
import net.minecraft.world.level.StructureManager;
import net.minecraft.world.level.WorldGenLevel;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.chunk.ChunkGenerator;
import net.minecraft.world.level.levelgen.NoiseBasedChunkGenerator;
import net.minecraft.world.level.levelgen.RandomState;
import net.minecraft.world.level.levelgen.blending.Blender;
import net.minecraft.world.level.levelgen.placement.FeaturePlacer;
import net.minecraft.world.level.levelgen.placement.PlacedFeature;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.Redirect;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/** Replaces the biome stage with the native one when the world qualifies. */
@Mixin(ChunkGenerator.class)
public abstract class ChunkGeneratorMixin {
	@Inject(method = "applyBiomeDecoration", at = @At("HEAD"))
	private void painite$oreBegin(WorldGenLevel level, ChunkAccess chunk, StructureManager structureManager, CallbackInfo ci) {
		OreBridge.begin(level, chunk);
	}

	@Inject(method = "applyBiomeDecoration", at = @At(value = "INVOKE",
			target = "Lnet/minecraft/world/level/StructureManager;startsForStructure(IILnet/minecraft/world/level/levelgen/structure/Structure;)Ljava/util/List;"))
	private void painite$oreFlushBeforeStructure(WorldGenLevel level, ChunkAccess chunk, StructureManager structureManager, CallbackInfo ci) {
		OreBridge.flush();
	}

	@Redirect(method = "applyBiomeDecoration", at = @At(value = "INVOKE",
			target = "Lnet/minecraft/world/level/levelgen/placement/FeaturePlacer;placeWithBiomeCheck(Lnet/minecraft/world/level/levelgen/placement/PlacedFeature;Lnet/minecraft/util/RandomSource;Lnet/minecraft/core/BlockPos;)Z"))
	private boolean painite$oreBatch(FeaturePlacer placer, PlacedFeature feature, RandomSource random, BlockPos origin) {
		if (!FeatureProbe.ENABLED) {
			return OreBridge.place(placer, feature, random, origin);
		}
		long t0 = System.nanoTime();
		boolean result = false;
		try {
			result = OreBridge.place(placer, feature, random, origin);
			return result;
		} finally {
			FeatureProbe.of(feature).stop(t0, result);
		}
	}

	@Inject(method = "applyBiomeDecoration", at = @At("RETURN"))
	private void painite$oreEnd(WorldGenLevel level, ChunkAccess chunk, StructureManager structureManager, CallbackInfo ci) {
		OreBridge.end();
	}

	@WrapMethod(method = "doCreateBiomes")
	private void painite$timeBiomes(Blender blender, RandomState randomState, ChunkAccess protoChunk, Operation<Void> original) {
		BusyProbe p = BusyProbe.of("terrain.biomes.total");
		long t0 = p.start();
		try {
			original.call(blender, randomState, protoChunk);
		} finally {
			p.stop(t0);
		}
	}

	@Inject(method = "doCreateBiomes", at = @At("HEAD"), cancellable = true)
	private void painite$nativeBiomes(Blender blender, RandomState randomState, ChunkAccess protoChunk, CallbackInfo ci) {
		if (!TerrainBridge.ENABLED || !blender.isEmpty()) {
			return;
		}
		if (!((Object) this instanceof NoiseBasedChunkGenerator generator) || !TerrainBridge.serves(generator)) {
			return;
		}
		if (TerrainBridge.fillBiomes(protoChunk)) {
			ci.cancel();
		}
	}
}
