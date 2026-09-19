package me.apika.painite.terrain;

/** Implemented on NoiseChunk by mixin: whether the native path may replace doFill, and whether it did. */
public interface PainiteNoiseChunk {
	/** True when the chunk has no blending, the one input the native fill lacks. */
	boolean painite$eligible();

	/** The structure pieces bearding this chunk in the native's flat layout, or null when there are none. */
	int[] painite$beard();

	/** Set by the doFill hook when the native holds this chunk's fill. */
	void painite$setNativeFilled(boolean filled);

	boolean painite$nativeFilled();

	/** Set by the buildSurface hook when the native surface pass carved the chunk too. */
	void painite$setNativeCarved(boolean carved);

	boolean painite$nativeCarved();
}
