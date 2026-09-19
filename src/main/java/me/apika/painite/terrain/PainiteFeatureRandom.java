package me.apika.painite.terrain;

/** Implemented on WorldgenRandom by mixin: the arguments of the last setFeatureSeed call. */
public interface PainiteFeatureRandom {
	long painite$decorationSeed();

	int painite$featureIndex();

	int painite$featureStep();
}
