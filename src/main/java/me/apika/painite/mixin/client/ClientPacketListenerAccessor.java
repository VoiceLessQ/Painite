package me.apika.painite.mixin.client;

import net.minecraft.client.multiplayer.ClientPacketListener;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.gen.Accessor;

/** The chunk radius the server actually sends, which bounds where real terrain can be. */
@Mixin(ClientPacketListener.class)
public interface ClientPacketListenerAccessor {
	@Accessor("serverChunkRadius")
	int painite$serverChunkRadius();
}
