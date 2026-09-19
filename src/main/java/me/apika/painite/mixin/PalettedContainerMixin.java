package me.apika.painite.mixin;

import com.llamalad7.mixinextras.injector.wrapmethod.WrapMethod;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import java.util.Arrays;
import net.minecraft.util.BitStorage;
import net.minecraft.util.ZeroBitStorage;
import net.minecraft.world.level.chunk.Palette;
import net.minecraft.world.level.chunk.PaletteResize;
import net.minecraft.world.level.chunk.PalettedContainer;
import org.spongepowered.asm.mixin.Mixin;

/**
 * Re-encoding a single-value section for serialisation unpacks 4096
 * zeros and maps each through the palettes; the answer is one palette
 * entry. Behind -Dpainite.fastPack=true for A/B runs; output identical.
 */
@Mixin(PalettedContainer.class)
public abstract class PalettedContainerMixin {
	private static final boolean FAST_PACK = Boolean.getBoolean("painite.fastPack");

	@WrapMethod(method = "reencodeContents")
	private static <T> int[] painite$reencodeUniform(BitStorage storage, Palette<T> oldPalette, Palette<T> newPalette, Operation<int[]> original) {
		if (!FAST_PACK || !(storage instanceof ZeroBitStorage)) {
			return original.call(storage, oldPalette, newPalette);
		}
		int[] ids = new int[storage.getSize()];
		int id = newPalette.idFor(oldPalette.valueFor(0), PaletteResize.noResizeExpected());
		if (id != 0) {
			Arrays.fill(ids, id);
		}
		return ids;
	}
}
