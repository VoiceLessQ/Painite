package me.apika.painite.mixin;

import com.llamalad7.mixinextras.injector.wrapmethod.WrapMethod;
import com.llamalad7.mixinextras.injector.wrapoperation.Operation;
import me.apika.painite.probe.BusyProbe;
import net.minecraft.nbt.CompoundTag;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.world.level.chunk.ChunkAccess;
import net.minecraft.world.level.chunk.storage.SerializableChunkData;
import org.spongepowered.asm.mixin.Mixin;

/** Diagnostic only: busy time of chunk serialisation (snapshot on the main thread, NBT write on the pool). */
@Mixin(SerializableChunkData.class)
public abstract class SerializableChunkDataMixin {
	@WrapMethod(method = "copyOf")
	private static SerializableChunkData painite$timeCopy(ServerLevel level, ChunkAccess chunk, Operation<SerializableChunkData> original) {
		BusyProbe p = BusyProbe.of("serialise.copyOf");
		long t0 = p.start();
		try {
			return original.call(level, chunk);
		} finally {
			p.stop(t0);
		}
	}

	@WrapMethod(method = "write")
	private CompoundTag painite$timeWrite(Operation<CompoundTag> original) {
		BusyProbe p = BusyProbe.of("serialise.write");
		long t0 = p.start();
		try {
			return original.call();
		} finally {
			p.stop(t0);
		}
	}
}
