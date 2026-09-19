package me.apika.painite.mixin.client;

import com.mojang.renderpearl.api.commands.RenderPass;
import me.apika.painite.lod.FrameProbe;
import me.apika.painite.lod.LodRenderer;
import net.minecraft.client.renderer.LevelRenderer;
import net.minecraft.client.renderer.chunk.ChunkSectionsToRender;
import net.minecraft.client.renderer.feature.FeatureRenderDispatcher;
import net.minecraft.client.renderer.state.level.LevelRenderState;
import org.spongepowered.asm.mixin.Final;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Shadow;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/** Times each level render when the frame probe is on, and draws the far view after the solid terrain. */
@Mixin(LevelRenderer.class)
public abstract class LevelRendererMixin {
	@Shadow
	@Final
	private LevelRenderState levelRenderState;

	@Inject(method = "render", at = @At("HEAD"))
	private void painite$renderBegin(CallbackInfo ci) {
		if (FrameProbe.ENABLED) {
			FrameProbe.begin();
		}
		LodRenderer.prepare(this.levelRenderState.cameraRenderState);
	}

	@Inject(method = "render", at = @At("RETURN"))
	private void painite$renderEnd(CallbackInfo ci) {
		if (FrameProbe.ENABLED) {
			FrameProbe.end();
		}
	}

	@Inject(method = "executeSolid", at = @At("RETURN"))
	private void painite$drawFarView(ChunkSectionsToRender sections, FeatureRenderDispatcher.PreparedFrame featureFrame, RenderPass renderPass, CallbackInfo ci) {
		LodRenderer.draw(renderPass, this.levelRenderState.cameraRenderState);
	}
}
