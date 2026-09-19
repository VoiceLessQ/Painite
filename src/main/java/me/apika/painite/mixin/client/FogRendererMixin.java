package me.apika.painite.mixin.client;

import me.apika.painite.lod.LodRenderer;
import net.minecraft.client.renderer.fog.FogData;
import net.minecraft.client.renderer.fog.FogRenderer;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

/** Moves the render-distance fog out to the far-view radius, so the last real chunks do not fade to the sky in front of the far mesh. */
@Mixin(FogRenderer.class)
public abstract class FogRendererMixin {
	@Inject(method = "setupFog", at = @At("RETURN"))
	private void painite$farFog(CallbackInfoReturnable<FogData> cir) {
		float far = LodRenderer.farPlaneBlocks();
		if (far <= 0f) {
			return;
		}
		FogData fog = cir.getReturnValue();
		fog.renderDistanceStart = Math.max(fog.renderDistanceStart, far * LodRenderer.FOG_CLEAR);
		fog.renderDistanceEnd = Math.max(fog.renderDistanceEnd, far);
	}
}
