package me.apika.painite.lod;

import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicLong;
import me.apika.painite.PainiteNative;
import me.apika.painite.terrain.TerrainBridge;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerTickEvents;
import net.fabricmc.fabric.api.networking.v1.ServerPlayConnectionEvents;
import net.fabricmc.fabric.api.networking.v1.ServerPlayNetworking;
import net.minecraft.server.level.ServerPlayer;
import net.minecraft.world.level.ChunkPos;
import net.minecraft.world.level.Level;

/**
 * Streams far-view records to players that run the mod. The Server
 * thread only samples positions; batches are built by the native and
 * handed to Netty from one daemon thread.
 */
public final class LodSender {
	/** Chunks around each player that get a record, -Dpainite.lodRadius. */
	public static final int RADIUS = Integer.getInteger("painite.lodRadius", 64);
	/** Within this many chunks the record is every column; beyond, one cell per 4x4 columns, -Dpainite.lodNear. */
	public static final int NEAR = Integer.getInteger("painite.lodNear", 48);
	/** Chunks per batch, ten batches a second, -Dpainite.lodRate. */
	public static final int RATE = Integer.getInteger("painite.lodRate", 128);
	private static final long PERIOD_MS = 100;
	private static final int SAMPLE_EVERY = 5;

	private record Sample(int chunkX, int chunkZ) {}

	private static final Map<ServerPlayer, Sample> TRACKED = new ConcurrentHashMap<>();
	/** Players that got the palette and have not yet said what they hold, by join time. */
	private static final Map<ServerPlayer, Long> PENDING = new ConcurrentHashMap<>();
	/** A client that never reports what it holds is served anyway after this long. */
	private static final long PENDING_MS = 10_000;
	private static final AtomicLong BATCHES = new AtomicLong();
	private static final AtomicLong CHUNKS = new AtomicLong();
	private static final AtomicLong BYTES = new AtomicLong();
	private static final AtomicLong BUILD_NS = new AtomicLong();
	private static final AtomicLong SEND_NS = new AtomicLong();
	private static volatile Thread thread;
	private static volatile boolean running;
	private static int tick;

	private LodSender() {}

	public static void register() {
		ServerPlayConnectionEvents.JOIN.register((listener, sender, server) -> onJoin(listener.player));
		ServerPlayConnectionEvents.DISCONNECT.register((listener, server) -> onLeave(listener.player));
		ServerTickEvents.END_SERVER_TICK.register(server -> {
			if (++tick % SAMPLE_EVERY == 0) {
				sample();
			}
		});
		ServerLifecycleEvents.SERVER_STOPPING.register(server -> stop());
		ServerPlayNetworking.registerGlobalReceiver(LodPayloads.Have.TYPE, (payload, context) -> {
			ServerPlayer player = context.player();
			if (payload.bitmap().length != LodPayloads.HAVE_BYTES || !(TRACKED.containsKey(player) || PENDING.containsKey(player))) {
				return;
			}
			PainiteNative.terrainLodHave(player.getId(), payload.rx(), payload.rz(), payload.bitmap());
			if (payload.last()) {
				track(player);
			}
		});
	}

	private static void onJoin(ServerPlayer player) {
		if (!TerrainBridge.lodActive() || !ServerPlayNetworking.canSend(player, LodPayloads.Batch.TYPE)) {
			return;
		}
		ServerPlayNetworking.send(player, new LodPayloads.Palette(LodPayloads.PROTOCOL, TerrainBridge.paletteStrings(), TerrainBridge.biomeIds(), RADIUS, TerrainBridge.worldId(), LodPalette.extra()));
		PENDING.put(player, System.currentTimeMillis());
	}

	/** Server thread: new extra palette names go to every far-view client. */
	public static void broadcastExtra(int start, java.util.List<String> names) {
		LodPayloads.PaletteExtra payload = new LodPayloads.PaletteExtra(start, names);
		for (ServerPlayer player : TRACKED.keySet()) {
			ServerPlayNetworking.send(player, payload);
		}
		for (ServerPlayer player : PENDING.keySet()) {
			ServerPlayNetworking.send(player, payload);
		}
	}

	/** The client has said what it holds (or never will): start sending. */
	private static void track(ServerPlayer player) {
		if (PENDING.remove(player) == null || player.hasDisconnected()) {
			return;
		}
		ChunkPos pos = player.chunkPosition();
		TRACKED.put(player, new Sample(pos.x(), pos.z()));
		start();
	}

	private static void onLeave(ServerPlayer player) {
		PENDING.remove(player);
		if (TRACKED.remove(player) != null) {
			PainiteNative.terrainLodForget(player.getId());
		}
	}

	/** Server thread: the chunk each tracked player stands in. */
	private static void sample() {
		long now = System.currentTimeMillis();
		for (Map.Entry<ServerPlayer, Long> entry : PENDING.entrySet()) {
			if (now - entry.getValue() > PENDING_MS) {
				track(entry.getKey());
			}
		}
		for (Map.Entry<ServerPlayer, Sample> entry : TRACKED.entrySet()) {
			ServerPlayer player = entry.getKey();
			if (player.level().dimension() != Level.OVERWORLD) {
				continue;
			}
			ChunkPos pos = player.chunkPosition();
			if (pos.x() != entry.getValue().chunkX() || pos.z() != entry.getValue().chunkZ()) {
				entry.setValue(new Sample(pos.x(), pos.z()));
			}
		}
	}

	private static synchronized void start() {
		if (thread != null) {
			return;
		}
		running = true;
		Thread t = new Thread(LodSender::loop, "Painite-LodSender");
		t.setDaemon(true);
		thread = t;
		t.start();
	}

	private static synchronized void stop() {
		running = false;
		Thread t = thread;
		thread = null;
		if (t != null) {
			t.interrupt();
		}
		TRACKED.clear();
		PENDING.clear();
	}

	private static void loop() {
		while (running) {
			try {
				Thread.sleep(PERIOD_MS);
			} catch (InterruptedException e) {
				return;
			}
			for (Map.Entry<ServerPlayer, Sample> entry : TRACKED.entrySet()) {
				ServerPlayer player = entry.getKey();
				Sample at = entry.getValue();
				if (player.level().dimension() != Level.OVERWORLD || player.hasDisconnected()) {
					continue;
				}
				long t0 = System.nanoTime();
				byte[] batch = PainiteNative.terrainLodBatch(player.getId(), at.chunkX(), at.chunkZ(), RADIUS, NEAR, RATE);
				long t1 = System.nanoTime();
				BUILD_NS.addAndGet(t1 - t0);
				if (batch == null) {
					continue;
				}
				ServerPlayNetworking.send(player, new LodPayloads.Batch(batch));
				SEND_NS.addAndGet(System.nanoTime() - t1);
				BATCHES.incrementAndGet();
				CHUNKS.addAndGet(Math.max(0, LodPayloads.countEntries(batch)));
				BYTES.addAndGet(batch.length);
			}
		}
	}

	/** Players being served, for the backfill walk. */
	public static java.util.List<ServerPlayer> players() {
		return new java.util.ArrayList<>(TRACKED.keySet());
	}

	public static String report() {
		long batches = BATCHES.get();
		return "[painite] far-view send: " + CHUNKS.get() + " chunks (" + BYTES.get() / 1024 + " KB) in " + batches + " batches to " + TRACKED.size()
				+ " players, radius " + RADIUS + " full to " + NEAR + ", build " + BUILD_NS.get() / 1_000_000L + " ms, hand-off " + SEND_NS.get() / 1_000_000L + " ms, " + LodBackfill.report();
	}
}
