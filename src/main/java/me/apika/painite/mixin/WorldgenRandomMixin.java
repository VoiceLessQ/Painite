package me.apika.painite.mixin;

import me.apika.painite.terrain.PainiteFeatureRandom;
import net.minecraft.world.level.levelgen.WorldgenRandom;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Unique;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/** Remembers the last feature seed so a batched feature can be reseeded natively or for a shadow run. */
@Mixin(WorldgenRandom.class)
public abstract class WorldgenRandomMixin implements PainiteFeatureRandom {
	@Unique
	private long painite$decorationSeed;
	@Unique
	private int painite$featureIndex;
	@Unique
	private int painite$featureStep;

	@Inject(method = "setFeatureSeed", at = @At("HEAD"))
	private void painite$remember(long seed, int index, int step, CallbackInfo ci) {
		this.painite$decorationSeed = seed;
		this.painite$featureIndex = index;
		this.painite$featureStep = step;
	}

	@Override
	public long painite$decorationSeed() {
		return this.painite$decorationSeed;
	}

	@Override
	public int painite$featureIndex() {
		return this.painite$featureIndex;
	}

	@Override
	public int painite$featureStep() {
		return this.painite$featureStep;
	}
}
