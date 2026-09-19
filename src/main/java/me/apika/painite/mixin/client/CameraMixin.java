package me.apika.painite.mixin.client;

import me.apika.painite.lod.LodRenderer;
import net.minecraft.client.Camera;
import org.objectweb.asm.Opcodes;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Shadow;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/** Pushes the far plane out to the far-view draw radius so the far mesh is not clipped. */
@Mixin(Camera.class)
public abstract class CameraMixin {
	@Shadow
	private float depthFar;

	@Inject(method = "update", at = @At(value = "FIELD", target = "Lnet/minecraft/client/Camera;depthFar:F", opcode = Opcodes.PUTFIELD, shift = At.Shift.AFTER))
	private void painite$farPlane(CallbackInfo ci) {
		this.depthFar = Math.max(this.depthFar, LodRenderer.farPlaneBlocks());
	}
}
