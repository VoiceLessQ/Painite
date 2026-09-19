package me.apika.painite.mixin;

import java.util.List;
import net.minecraft.world.level.levelgen.Beardifier;
import net.minecraft.world.level.levelgen.structure.BoundingBox;
import net.minecraft.world.level.levelgen.structure.pools.JigsawJunction;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

/** The pieces a Beardifier was built from, so the native fill can apply the same terrain adaptation. */
@Mixin(Beardifier.class)
public interface BeardifierAccessor {
	@Accessor("pieces")
	List<Beardifier.Rigid> painite$pieces();

	@Accessor("junctions")
	List<JigsawJunction> painite$junctions();

	@Accessor("affectedBox")
	BoundingBox painite$affectedBox();
}
