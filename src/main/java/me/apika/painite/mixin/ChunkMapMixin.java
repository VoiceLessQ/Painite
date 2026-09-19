package me.apika.painite.mixin;

import java.util.concurrent.CompletableFuture;

import com.llamalad7.mixinextras.injector.wrapmethod.WrapMethod;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import me.apika.painite.probe.StepProbe;
import net.minecraft.server.level.ChunkMap;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.chunk.status.ChunkStatus;
import org.spongepowered.asm.mixin.Mixin;

/** Diagnostic only: the EMPTY step is a load round trip that never reaches ChunkStep.apply. */
@Mixin(ChunkMap.class)
public abstract class ChunkMapMixin {
	@WrapMethod(method = "scheduleChunkLoad")
	private CompletableFuture<ChunkAccess> painite$timeLoad(ChunkPos pos, Operation<CompletableFuture<ChunkAccess>> original) {
		StepProbe probe = StepProbe.of(ChunkStatus.EMPTY);
		long t0 = System.nanoTime();
		probe.started();
		return original.call(pos).whenComplete((c, e) -> probe.completed(System.nanoTime() - t0));
	}
}
