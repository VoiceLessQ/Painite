package me.apika.painite.mixin;

import java.util.concurrent.CompletableFuture;

import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import com.llamalad7.mixinextras.injector.wrapoperation.WrapOperation;
import me.apika.painite.probe.DispatcherProbe;
import net.minecraft.server.level.ChunkTaskDispatcher;
import net.minecraft.util.thread.TaskScheduler;
import org.spongepowered.asm.mixin.Final;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Shadow;
import org.spongepowered.asm.mixin.Unique;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;

/** Diagnostic only: times the dispatcher's pop, run and poll-again cycle. */
@Mixin(ChunkTaskDispatcher.class)
public abstract class ChunkTaskDispatcherMixin {
	@Shadow @Final private TaskScheduler<Runnable> executor;

	@Unique
	private DispatcherProbe painite$probe() {
		return DispatcherProbe.of(executor.name());
	}
	@Inject(method = "popTasks", at = @At("HEAD"))
	private void painite$popped(org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable<?> cir) {
		painite$probe().popped();
	}

	@Inject(method = "submit", at = @At("HEAD"))
	private void painite$submitted(CallbackInfo ci) {
		painite$probe().submitted();
	}

	@Inject(method = "onLevelChange", at = @At("HEAD"))
	private void painite$levelChanged(CallbackInfo ci) {
		painite$probe().levelChanged();
	}

	@WrapOperation(
			method = "scheduleForExecution",
			at = @At(value = "INVOKE", target = "Ljava/util/concurrent/CompletableFuture;allOf([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;"))
	private CompletableFuture<Void> painite$cycle(CompletableFuture<?>[] futures, Operation<CompletableFuture<Void>> original) {
		long t0 = System.nanoTime();
		DispatcherProbe probe = painite$probe();
		return original.call((Object) futures).whenComplete((r, e) -> probe.cycleDone(System.nanoTime() - t0));
	}

	@WrapOperation(
			method = "lambda$scheduleForExecution$1",
			at = @At(value = "INVOKE", target = "Ljava/lang/Runnable;run()V"))
	private static void painite$run(Runnable task, Operation<Void> original) {
		long t0 = System.nanoTime();
		try {
			original.call(task);
		} finally {
			// Static lambda: no instance, so both dispatchers land in WORLDGEN below.
			DispatcherProbe.WORLDGEN.ran(System.nanoTime() - t0);
		}
	}
}
