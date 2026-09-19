package me.apika.painite.mixin;

import java.util.concurrent.CompletableFuture;

import com.llamalad7.mixinextras.injector.wrapmethod.WrapMethod;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import me.apika.painite.probe.StepProbe;
import net.minecraft.server.level.GenerationChunkHolder;
import net.minecraft.util.StaticCache2D;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.chunk.status.ChunkStep;
import net.minecraft.world.level.chunk.status.WorldGenContext;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Shadow;

/** Diagnostic only: times every status step from schedule to completion. */
@Mixin(ChunkStep.class)
public abstract class ChunkStepMixin {
	@Shadow public abstract net.minecraft.world.level.chunk.status.ChunkStatus targetStatus();

	@WrapMethod(method = "apply")
	private CompletableFuture<ChunkAccess> painite$time(
			WorldGenContext context, StaticCache2D<GenerationChunkHolder> cache, ChunkAccess chunk,
			Operation<CompletableFuture<ChunkAccess>> original) {
		if (!chunk.getPersistedStatus().isBefore(targetStatus())) {
			return original.call(context, cache, chunk);
		}
		StepProbe probe = StepProbe.of(targetStatus());
		long t0 = System.nanoTime();
		probe.started();
		return original.call(context, cache, chunk).whenComplete((c, e) -> probe.completed(System.nanoTime() - t0));
	}
}
