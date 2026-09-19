package me.apika.painite.lod;

import java.util.List;
import me.apika.painite.PainiteMod;
import net.fabricmc.fabric.api.networking.v1.PayloadTypeRegistry;
import net.minecraft.network.codec.ByteBufCodecs;
import net.minecraft.network.codec.StreamCodec;
import net.minecraft.network.protocol.common.custom.CustomPacketPayload;
import net.minecraft.resources.Identifier;

/** The two clientbound far-view payloads. Both ends check the version before reading a batch. */
public final class LodPayloads {
	/** Bumped when a record or batch layout changes. */
	public static final int PROTOCOL = 6;
	/** Bytes in a region's have bitmap: one bit per chunk of 32x32. */
	public static final int HAVE_BYTES = 128;
	/** One batch carries at most this many bytes; 32 chunks is 49 KB. */
	public static final int MAX_BATCH_BYTES = 1 << 20;
	/** Bytes of a batch entry's header: chunk x, chunk z, flags (stage in the low byte, bit 8 for a coarse record). */
	public static final int HEADER_BYTES = 12;
	public static final int COARSE_FLAG = 1 << 8;
	public static final int RECORD_BYTES = 2304;
	public static final int COARSE_BYTES = 144;

	/** Entries in a batch as the native lays them out; -1 for a torn one. Shared by both sides. */
	public static int countEntries(byte[] batch) {
		int at = 0;
		int count = 0;
		while (at < batch.length) {
			if (at + HEADER_BYTES > batch.length) {
				return -1;
			}
			int flags = (batch[at + 8] & 0xff) | (batch[at + 9] & 0xff) << 8;
			at += HEADER_BYTES + ((flags & COARSE_FLAG) != 0 ? COARSE_BYTES : RECORD_BYTES);
			count++;
		}
		return at == batch.length ? count : -1;
	}

	private LodPayloads() {}

	/** Sent once on join: the protocol version and the names the record's palette ids and biome indices refer to. */
	public record Palette(int version, List<String> blocks, List<String> biomes, int farRadius, byte[] worldId, List<String> extra) implements CustomPacketPayload {
		public static final Type<Palette> TYPE = new Type<>(Identifier.fromNamespaceAndPath(PainiteMod.MOD_ID, "lod_palette"));
		public static final StreamCodec<io.netty.buffer.ByteBuf, Palette> CODEC = StreamCodec.composite(
				ByteBufCodecs.VAR_INT, Palette::version,
				ByteBufCodecs.STRING_UTF8.apply(ByteBufCodecs.list()), Palette::blocks,
				ByteBufCodecs.STRING_UTF8.apply(ByteBufCodecs.list()), Palette::biomes,
				ByteBufCodecs.VAR_INT, Palette::farRadius,
				ByteBufCodecs.byteArray(16), Palette::worldId,
				ByteBufCodecs.STRING_UTF8.apply(ByteBufCodecs.list()), Palette::extra,
				Palette::new);

		@Override
		public Type<Palette> type() {
			return TYPE;
		}
	}

	/** Chunk records as the native writes them: chunk x, chunk z, flags (little-endian ints; stage in the low byte,
	 *  bit 8 for a coarse record), then 1536 record bytes or 64 coarse bytes, repeated. */
	public record Batch(byte[] entries) implements CustomPacketPayload {
		public static final Type<Batch> TYPE = new Type<>(Identifier.fromNamespaceAndPath(PainiteMod.MOD_ID, "lod_batch"));
		public static final StreamCodec<io.netty.buffer.ByteBuf, Batch> CODEC =
				ByteBufCodecs.byteArray(MAX_BATCH_BYTES).map(Batch::new, Batch::entries);

		@Override
		public Type<Batch> type() {
			return TYPE;
		}
	}

	/** Register both payload types; runs on the server and the client. */
	/** Server to client: names appended to the extra palette, ids from `LodPalette.EXTRA_BASE + start`. */
	public record PaletteExtra(int start, List<String> names) implements CustomPacketPayload {
		public static final Type<PaletteExtra> TYPE = new Type<>(Identifier.fromNamespaceAndPath(PainiteMod.MOD_ID, "lod_palette_extra"));
		public static final StreamCodec<io.netty.buffer.ByteBuf, PaletteExtra> CODEC = StreamCodec.composite(
				ByteBufCodecs.VAR_INT, PaletteExtra::start,
				ByteBufCodecs.STRING_UTF8.apply(ByteBufCodecs.list()), PaletteExtra::names,
				PaletteExtra::new);

		@Override
		public Type<PaletteExtra> type() {
			return TYPE;
		}
	}

	/** Client to server: the chunks of one region the client already holds, so they are not sent again;
	 *  `last` closes a report and lets the server start sending. */
	public record Have(int rx, int rz, byte[] bitmap, boolean last) implements CustomPacketPayload {
		public static final Type<Have> TYPE = new Type<>(Identifier.fromNamespaceAndPath(PainiteMod.MOD_ID, "lod_have"));
		public static final StreamCodec<io.netty.buffer.ByteBuf, Have> CODEC = StreamCodec.composite(
				ByteBufCodecs.VAR_INT, Have::rx,
				ByteBufCodecs.VAR_INT, Have::rz,
				ByteBufCodecs.byteArray(HAVE_BYTES), Have::bitmap,
				ByteBufCodecs.BOOL, Have::last,
				Have::new);

		@Override
		public Type<Have> type() {
			return TYPE;
		}
	}

	public static void register() {
		PayloadTypeRegistry.clientboundPlay().register(Palette.TYPE, Palette.CODEC);
		PayloadTypeRegistry.clientboundPlay().register(PaletteExtra.TYPE, PaletteExtra.CODEC);
		PayloadTypeRegistry.serverboundPlay().register(Have.TYPE, Have.CODEC);
		PayloadTypeRegistry.clientboundPlay().register(Batch.TYPE, Batch.CODEC);
	}
}
