package me.apika.painite.mixin;

import me.apika.painite.terrain.PainiteNoiseChunk;
import me.apika.painite.terrain.TerrainBridge;
import net.minecraft.world.level.levelgen.Aquifer;
import net.minecraft.world.level.levelgen.Beardifier;
import net.minecraft.world.level.levelgen.NoiseChunk;
import net.minecraft.world.level.levelgen.NoiseGeneratorSettings;
import net.minecraft.world.level.levelgen.RandomState;
import net.minecraft.world.level.levelgen.blending.Blender;
import net.minecraft.world.level.levelgen.densityfunction.DensityVolume;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Unique;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

@Mixin(NoiseChunk.class)
public abstract class NoiseChunkMixin implements PainiteNoiseChunk {
	@Unique
	private boolean painite$eligible;
	@Unique
	private int[] painite$beard;
	@Unique
	private boolean painite$nativeFilled;
	@Unique
	private boolean painite$nativeCarved;

	@Inject(method = "<init>", at = @At("TAIL"))
	private void painite$remember(RandomState randomState, Beardifier beardifier, NoiseGeneratorSettings settings,
			Aquifer.FluidPicker fluidPicker, Blender blender, DensityVolume volume, CallbackInfo ci) {
		this.painite$eligible = blender.isEmpty();
		this.painite$beard = TerrainBridge.beardPieces(beardifier);
	}

	@Override
	public boolean painite$eligible() {
		return this.painite$eligible;
	}

	@Override
	public void painite$decline() {
		this.painite$eligible = false;
		this.painite$nativeFilled = false;
	}

	@Override
	public int[] painite$beard() {
		return this.painite$beard;
	}

	@Override
	public void painite$setNativeFilled(boolean filled) {
		this.painite$nativeFilled = filled;
	}

	@Override
	public boolean painite$nativeFilled() {
		return this.painite$nativeFilled;
	}

	@Override
	public void painite$setNativeCarved(boolean carved) {
		this.painite$nativeCarved = carved;
	}

	@Override
	public boolean painite$nativeCarved() {
		return this.painite$nativeCarved;
	}
}
