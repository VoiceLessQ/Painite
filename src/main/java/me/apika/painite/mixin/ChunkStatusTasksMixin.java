package me.apika.painite.mixin;

import java.util.concurrent.CompletableFuture;

import com.llamalad7.mixinextras.injector.wrapmethod.WrapMethod;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import me.apika.painite.sched.StageGate;
import net.minecraft.server.level.GenerationChunkHolder;
import net.minecraft.util.StaticCache2D;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.chunk.status.ChunkStatusTasks;
import net.minecraft.world.level.chunk.status.ChunkStep;
import net.minecraft.world.level.chunk.status.WorldGenContext;
import org.spongepowered.asm.mixin.Mixin;

/**
 * Moves the four serial stage bodies onto the Painite pool behind the
 * native gate. Other mods' injections into these bodies still run, inside
 * the wrapped call.
 */
@Mixin(ChunkStatusTasks.class)
abstract class ChunkStatusTasksMixin {
	@WrapMethod(method = "generateStructureStarts")
	private static CompletableFuture<ChunkAccess> painite$starts(
			WorldGenContext context, ChunkStep step, StaticCache2D<GenerationChunkHolder> chunks, ChunkAccess chunk,
			Operation<CompletableFuture<ChunkAccess>> original) {
		return gate(StageGate.STRUCTURE_STARTS, context, step, chunks, chunk, original);
	}

	@WrapMethod(method = "generateStructureReferences")
	private static CompletableFuture<ChunkAccess> painite$references(
			WorldGenContext context, ChunkStep step, StaticCache2D<GenerationChunkHolder> chunks, ChunkAccess chunk,
			Operation<CompletableFuture<ChunkAccess>> original) {
		return gate(StageGate.STRUCTURE_REFERENCES, context, step, chunks, chunk, original);
	}

	@WrapMethod(method = "generateFeatures")
	private static CompletableFuture<ChunkAccess> painite$features(
			WorldGenContext context, ChunkStep step, StaticCache2D<GenerationChunkHolder> chunks, ChunkAccess chunk,
			Operation<CompletableFuture<ChunkAccess>> original) {
		return gate(StageGate.FEATURES, context, step, chunks, chunk, original);
	}

	@WrapMethod(method = "generateSpawn")
	private static CompletableFuture<ChunkAccess> painite$spawn(
			WorldGenContext context, ChunkStep step, StaticCache2D<GenerationChunkHolder> chunks, ChunkAccess chunk,
			Operation<CompletableFuture<ChunkAccess>> original) {
		return gate(StageGate.SPAWN, context, step, chunks, chunk, original);
	}

	private static CompletableFuture<ChunkAccess> gate(
			int stage, WorldGenContext context, ChunkStep step, StaticCache2D<GenerationChunkHolder> chunks,
			ChunkAccess chunk, Operation<CompletableFuture<ChunkAccess>> original) {
		if (!StageGate.enabled(stage)) {
			return StageGate.timed(stage, () -> original.call(context, step, chunks, chunk));
		}
		return StageGate.run(stage, chunks, chunk, () -> original.call(context, step, chunks, chunk));
	}
}
