package me.apika.painite.mixin;

import com.llamalad7.mixinextras.injector.wrapmethod.WrapMethod;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import me.apika.painite.probe.LightProbe;
import net.minecraft.server.level.ThreadedLevelLightEngine;
import org.spongepowered.asm.mixin.Mixin;

/** Diagnostic only: times the light executor's serial update batches. */
@Mixin(ThreadedLevelLightEngine.class)
public abstract class ThreadedLevelLightEngineMixin {
	@WrapMethod(method = "runUpdate")
	private void painite$time(Operation<Void> original) {
		long t0 = LightProbe.started();
		try {
			original.call();
		} finally {
			LightProbe.finished(t0);
		}
	}
}
