package me.apika.painite.lod;

import com.mojang.blaze3d.systems.RenderSystem;
import com.mojang.blaze3d.vertex.DefaultVertexFormat;
import com.mojang.renderpearl.api.buffers.GpuBuffer;
import com.mojang.renderpearl.api.buffers.GpuBufferSlice;
import com.mojang.renderpearl.api.commands.RenderPass;
import com.mojang.renderpearl.api.pipeline.ColorTargetState;
import com.mojang.renderpearl.api.pipeline.DepthStencilState;
import com.mojang.renderpearl.api.pipeline.PrimitiveTopology;
import com.mojang.renderpearl.api.pipeline.RenderPipeline;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import me.apika.painite.PainiteMod;
import me.apika.painite.PainiteNative;
import me.apika.painite.mixin.client.ClientPacketListenerAccessor;
import net.minecraft.client.Minecraft;
import com.mojang.blaze3d.buffers.Std140Builder;
import net.minecraft.client.renderer.BindGroupLayouts;
import net.minecraft.client.renderer.MappableRingBuffer;
import com.mojang.renderpearl.api.textures.FilterMode;
import net.minecraft.client.renderer.RenderPipelines;
import net.minecraft.client.renderer.fog.FogData;
import net.minecraft.client.renderer.fog.FogRenderer;
import net.minecraft.resources.Identifier;
import net.minecraft.client.renderer.state.level.CameraRenderState;
import net.minecraft.world.phys.AABB;
import net.minecraft.world.phys.Vec3;
import org.joml.Matrix4f;
import org.joml.Vector3f;
import org.joml.Vector4f;
import org.lwjgl.system.MemoryUtil;

/** Draws the far-view records as one position-colour mesh per 32x32 chunk region, after the solid terrain. */
public final class LodRenderer {
	public static final boolean ENABLED = Boolean.parseBoolean(System.getProperty("painite.lodDraw", "true"));
	/** -Dpainite.lodTint: draw the far mesh tinted magenta, to tell it from the game's chunks in a screenshot. */
	private static final boolean TINT = Boolean.getBoolean("painite.lodTint");
	/** Chunks the client draws from what it holds, on disk or received; -Dpainite.lodView. */
	public static final int VIEW = Integer.getInteger("painite.lodView", 256);
	/** Uploads per frame at most; a mesh is built off the render thread, only its upload happens here. */
	private static final int UPLOADS_PER_FRAME = 1;
	/** A region whose records keep changing is rebuilt at most this often. */
	private static final long REBUILD_DEBOUNCE_MS = 1500;
	private static final int REGION_BLOCKS = 512;
	/** Fraction of the far radius that stays clear before the far fog ramps; the fog mixin uses the same for real chunks. */
	public static final float FOG_CLEAR = 0.4f;
	private static final int VERTEX_BYTES = 16;
	private static final RenderPipeline PIPELINE = RenderPipelines.register(RenderPipeline.builder(RenderPipelines.MATRICES_FOG_SNIPPET)
			.withLocation(Identifier.fromNamespaceAndPath(PainiteMod.MOD_ID, "pipeline/far_view"))
			.withBindGroupLayout(BindGroupLayouts.SAMPLER2)
			.withVertexShader(Identifier.fromNamespaceAndPath(PainiteMod.MOD_ID, "core/far_view"))
			.withFragmentShader(Identifier.fromNamespaceAndPath(PainiteMod.MOD_ID, "core/far_view"))
			.withColorTargetState(ColorTargetState.DEFAULT)
			.withVertexBinding(0, DefaultVertexFormat.POSITION_COLOR)
			.withPrimitiveTopology(PrimitiveTopology.QUADS)
			.withDepthStencilState(DepthStencilState.DEFAULT)
			.withCull(true)
			.build());
	private static final Vector3f NO_OFFSET = new Vector3f();
	private static final Matrix4f IDENTITY = new Matrix4f();

	/** What a region's mesh was built from: records landed, and the scale of each 8x8-chunk block (4 bits each). */
	private record Key(long generation, long scales) {}

	/** A mesh the worker wrote into a pooled direct buffer, `length` bytes of it; null buffer for nothing to draw. */
	private record Built(Region region, Key key, ByteBuffer mesh, int length, int[] starts, int minY, int maxY, long ns) {}
	private static final int REGION_CHUNKS = 32;

	/** Direct buffers the worker fills and the render thread hands to the device, reused; at most this many idle. */
	private static final java.util.ArrayDeque<ByteBuffer> POOL = new java.util.ArrayDeque<>();
	private static final int POOL_IDLE = 4;
	private static final int FIRST_BUFFER_BYTES = 4 << 20;

	private static final class Region {
		final int rx;
		final int rz;
		Key key;
		/** The key a worker is building right now, or null. */
		Key building;
		long lastBuildMs;
		GpuBuffer buffer;
		int vertices;
		/** First vertex of each chunk (x + z * 32) and the total at the end, so a draw can skip the hole's chunks. */
		int[] starts;
		int minY = -64;
		int maxY = 320;

		Region(int rx, int rz) {
			this.rx = rx;
			this.rz = rz;
		}

		void close() {
			if (buffer != null) {
				buffer.close();
				buffer = null;
			}
			vertices = 0;
		}
	}

	private static final Map<Long, Region> REGIONS = new HashMap<>();
	private static final java.util.concurrent.ExecutorService BUILDER = java.util.concurrent.Executors.newSingleThreadExecutor(r -> {
		Thread t = new Thread(r, "Painite-LodMesh");
		t.setDaemon(true);
		return t;
	});
	private static final java.util.concurrent.ConcurrentLinkedQueue<Built> DONE = new java.util.concurrent.ConcurrentLinkedQueue<>();
	/** The game's fog with the far end moved out to the draw radius, one slot per frame in flight. */
	private static MappableRingBuffer fogBuffer;
	private static long drawnVertices;
	private static long drawnRegions;
	private static long rebuilds;
	private static long buildNs;
	private static long uploadNs;

	private LodRenderer() {}

	private static long regionKey(int rx, int rz) {
		return ((long) rx << 32) ^ (rz & 0xffffffffL);
	}

	private static GpuBufferSlice fogSlice;

	/** Chunks the mesh reaches: what the client holds within its own view, whatever the server sends. */
	private static int drawRadius(int view) {
		return Math.max(VIEW, view * 4);
	}

	/** Far plane the camera needs, in blocks; zero when nothing far is drawn. Also the reach of the fog. */
	public static float farPlaneBlocks() {
		return ENABLED && LodClient.ready() ? (drawRadius(Minecraft.getInstance().options.renderDistance().get()) + 1) * 16f : 0f;
	}

	/** Called at the start of the level render, before any pass is open: buffer writes are legal here. */
	public static void prepare(CameraRenderState camera) {
		if (!ENABLED || !LodClient.ready()) {
			fogSlice = null;
			return;
		}
		int view = Minecraft.getInstance().options.renderDistance().get();
		fogSlice = farFog(camera.fogData, drawRadius(view) * 16);
	}

	/** Called from the level renderer once the solid terrain of this frame is in the pass. */
	public static void draw(RenderPass renderPass, CameraRenderState camera) {
		if (!ENABLED || !LodClient.ready() || fogSlice == null) {
			return;
		}
		Minecraft minecraft = Minecraft.getInstance();
		Vec3 cam = camera.pos;
		int px = ((int) Math.floor(cam.x)) >> 4;
		int pz = ((int) Math.floor(cam.z)) >> 4;
		int view = minecraft.options.renderDistance().get();
		// Real chunks exist only where both the client's and the server's view distance reach; inside that box the
		// mesh is skipped exactly where the client holds the chunk, so neither a ring of nothing nor an overlap shows.
		// One more than the smaller view distance: the server sends and the client renders that border ring too.
		int hole = (minecraft.getConnection() == null ? view : Math.min(view, ((ClientPacketListenerAccessor) minecraft.getConnection()).painite$serverChunkRadius())) + 1;
		net.minecraft.client.multiplayer.ClientLevel level = minecraft.level;
		// The game draws the chunks of the effective view distance's circle (ChunkTrackingView.isInViewDistance, a
		// one-chunk buffer then a disc), fewer than it holds at the square's corners; the mesh covers the rest.
		circle = hole - 1;
		centreX = px;
		centreZ = pz;
		if (FrameProbe.ENABLED) {
			edgeRow(level, px, pz, hole);
		}
		int drawRadius = drawRadius(view);
		int rMin = (px - drawRadius) >> 5;
		int rMax = (px + drawRadius) >> 5;
		int zMin = (pz - drawRadius) >> 5;
		int zMax = (pz + drawRadius) >> 5;
		// Meshes built by the worker since last frame: upload a few, on this thread, where GPU calls belong.
		int uploaded = 0;
		Built built;
		while (uploaded < UPLOADS_PER_FRAME && (built = DONE.poll()) != null) {
			upload(built);
			uploaded++;
		}
		long nowMs = System.currentTimeMillis();
		List<Region> visible = new ArrayList<>();
		for (int rz = zMin; rz <= zMax; rz++) {
			for (int rx = rMin; rx <= rMax; rx++) {
				long generation = PainiteNative.lodClientRegionGeneration(rx, rz);
				if (generation == 0) {
					continue;
				}
				final int frx = rx;
				final int frz = rz;
				Region region = REGIONS.computeIfAbsent(regionKey(rx, rz), k -> new Region(frx, frz));
				Key key = new Key(generation, scalesFor(rx, rz, px, pz));
				boolean stale = !key.equals(region.key) && region.building == null;
				boolean settled = region.key == null || nowMs - region.lastBuildMs >= REBUILD_DEBOUNCE_MS || region.key.scales() != key.scales();
				if (stale && settled) {
					region.building = key;
					region.lastBuildMs = nowMs;
					BUILDER.execute(() -> build(region, key, px, pz));
				}
				if (region.vertices > 0) {
					visible.add(region);
				}
			}
		}
		// Regions that fell out of the draw radius give their buffers back.
		for (Iterator<Region> it = REGIONS.values().iterator(); it.hasNext(); ) {
			Region region = it.next();
			if (region.rx < rMin || region.rx > rMax || region.rz < zMin || region.rz > zMax) {
				region.close();
				it.remove();
			}
		}
		if (visible.isEmpty()) {
			return;
		}
		renderPass.pushDebugGroup(() -> "Painite far view");
		RenderSystem.bindDefaultUniforms(renderPass);
		renderPass.setUniform("Fog", fogSlice);
		renderPass.setPipeline(RenderSystem.getCompiledPipeline(PIPELINE));
		// The game's lightmap: sampled at full sky light and no block light, so night and rain darken the mesh like terrain.
		renderPass.setUniform("Sampler2", minecraft.gameRenderer.lightmap(), RenderSystem.getSamplerCache().getClampToEdge(FilterMode.LINEAR));
		int mostVertices = 0;
		for (Region region : visible) {
			mostVertices = Math.max(mostVertices, region.vertices);
		}
		RenderSystem.AutoStorageIndexBuffer indices = RenderSystem.getSequentialBuffer(PrimitiveTopology.QUADS);
		indices.requestIndexCount(mostVertices / 4 * 6);
		renderPass.setIndexBuffer(indices.getBuffer(), indices.type());
		Matrix4f modelView = RenderSystem.getModelViewMatrixCopy();
		for (Region region : visible) {
			double x0 = (double) region.rx * REGION_BLOCKS;
			double z0 = (double) region.rz * REGION_BLOCKS;
			AABB box = new AABB(x0, region.minY, z0, x0 + REGION_BLOCKS, region.maxY, z0 + REGION_BLOCKS);
			if (!camera.cullFrustum.isVisible(box)) {
				continue;
			}
			// The position-colour shader applies no model offset, so the region's corner goes into the matrix.
			Matrix4f regionView = new Matrix4f(modelView).translate((float) (x0 - cam.x), (float) -cam.y, (float) (z0 - cam.z));
			// The colour modulator's hole box is unused (half size 0); chunks are skipped by range below.
			Vector4f holeBox = new Vector4f(0f, 0f, 0f, TINT ? 1f : 0f);
			GpuBufferSlice transforms = RenderSystem.getDynamicUniforms().writeTransform(regionView, holeBox, NO_OFFSET, IDENTITY);
			renderPass.setUniform("DynamicTransforms", transforms);
			renderPass.setVertexBuffer(0, region.buffer.slice());
			// Loaded real chunks are skipped by vertex range, row by row, so their vertices never reach the GPU.
			int hx0 = Math.max(0, px - hole - (region.rx << 5));
			int hx1 = Math.min(REGION_CHUNKS - 1, px + hole - (region.rx << 5));
			int hz0 = Math.max(0, pz - hole - (region.rz << 5));
			int hz1 = Math.min(REGION_CHUNKS - 1, pz + hole - (region.rz << 5));
			if (hx0 > hx1 || hz0 > hz1) {
				drawRange(renderPass, 0, region.vertices);
			} else {
				int[] starts = region.starts;
				drawRange(renderPass, 0, starts[hz0 * REGION_CHUNKS]);
				for (int row = hz0; row <= hz1; row++) {
					int from = starts[row * REGION_CHUNKS];
					for (int lx = hx0; lx <= hx1; lx++) {
						int at = row * REGION_CHUNKS + lx;
						if (starts[at + 1] > starts[at] && covered(level, (region.rx << 5) + lx, (region.rz << 5) + row)) {
							drawRange(renderPass, from, starts[at]);
							from = starts[at + 1];
						}
					}
					drawRange(renderPass, from, starts[(row + 1) * REGION_CHUNKS]);
				}
				drawRange(renderPass, starts[(hz1 + 1) * REGION_CHUNKS], region.vertices);
			}
			drawnRegions++;
		}
		renderPass.popDebugGroup();
	}

	/** The view-distance circle the game renders this frame: radius in chunks and the camera's chunk. */
	private static int circle;
	private static int centreX;
	private static int centreZ;

	/** A chunk the game draws this frame: inside its view-distance circle, held, and with its eight neighbours held
	 *  and lit (SectionUpdateTracker.hasAllNeighbors, which gates compiling the sections). Mesh tops sit an eighth
	 *  under the block face, so where the game does draw it wins. */
	private static boolean covered(net.minecraft.client.multiplayer.ClientLevel level, int cx, int cz) {
		long ax = Math.max(0, Math.abs(cx - centreX) - 1);
		long az = Math.max(0, Math.abs(cz - centreZ) - 1);
		if (ax * ax + az * az >= (long) circle * circle || !level.getChunkSource().hasChunk(cx, cz)) {
			return false;
		}
		for (int dz = -1; dz <= 1; dz++) {
			for (int dx = -1; dx <= 1; dx++) {
				if ((dx != 0 || dz != 0) && (!level.getChunkSource().hasChunk(cx + dx, cz + dz) || !level.getLightEngine().lightOnInColumn(net.minecraft.core.SectionPos.getZeroNode(cx + dx, cz + dz)))) {
					return false;
				}
			}
		}
		return true;
	}

	/** Quads of the bound buffer from vertex v0 to v1 through the sequential index buffer. */
	private static void drawRange(RenderPass renderPass, int v0, int v1) {
		if (v1 > v0) {
			renderPass.drawIndexed((v1 - v0) / 4 * 6, 1, v0 / 4 * 6, 0, 0);
			drawnVertices += v1 - v0;
		}
	}

	/** The frame's fog with the render-distance ramp from 40% of the draw radius to its end; the shader curves it. */
	private static GpuBufferSlice farFog(FogData fog, float farBlocks) {
		if (fogBuffer == null) {
			fogBuffer = new MappableRingBuffer(() -> "Painite far view fog", GpuBuffer.USAGE_MAP_WRITE | GpuBuffer.USAGE_UNIFORM, FogRenderer.FOG_UBO_SIZE);
		}
		fogBuffer.rotate();
		try (GpuBufferSlice.MappedView view = fogBuffer.currentBuffer().slice().map(false, true)) {
			Std140Builder.intoBuffer(view.data())
					.putVec4(fog.color)
					.putFloat(fog.environmentalStart)
					.putFloat(fog.environmentalEnd)
					.putFloat(Math.max(fog.renderDistanceStart, farBlocks * FOG_CLEAR))
					.putFloat(Math.max(fog.renderDistanceEnd, farBlocks))
					.putFloat(fog.skyEnd)
					.putFloat(fog.cloudEnd);
		}
		return fogBuffer.currentBuffer().slice(0L, FogRenderer.FOG_UBO_SIZE);
	}

	/** Chunks per side of the blocks a scale is chosen for, as the native meshes them. */
	private static final int SCALE_BLOCK = 8;

	/** Cell size of the 8x8-chunk block at (bx, bz) from its nearest chunk distance to the player's chunk: 1 within 24
	 *  chunks, 2 within 48, 4 within 96, 8 within 192, 16 beyond. The native (scale_for) makes the same choice. */
	private static int scaleFor(int bx, int bz, int px, int pz) {
		int dx = Math.max(0, Math.max(bx * SCALE_BLOCK - px, px - (bx * SCALE_BLOCK + SCALE_BLOCK - 1)));
		int dz = Math.max(0, Math.max(bz * SCALE_BLOCK - pz, pz - (bz * SCALE_BLOCK + SCALE_BLOCK - 1)));
		int d = Math.max(dx, dz);
		return d < 24 ? 1 : d < 48 ? 2 : d < 96 ? 4 : d < 192 ? 8 : 16;
	}

	/** The scales of a region's sixteen blocks, log2 each in 4 bits, x fastest. */
	private static long scalesFor(int rx, int rz, int px, int pz) {
		long packed = 0;
		int blocks = REGION_CHUNKS / SCALE_BLOCK;
		for (int bz = 0; bz < blocks; bz++) {
			for (int bx = 0; bx < blocks; bx++) {
				int scale = scaleFor(rx * blocks + bx, rz * blocks + bz, px, pz);
				packed |= (long) Integer.numberOfTrailingZeros(scale) << ((bx + bz * blocks) * 4);
			}
		}
		return packed;
	}

	/** Worker thread: mesh the region natively into a pooled buffer; the result waits for the render thread. */
	private static void build(Region region, Key key, int px, int pz) {
		long t0 = System.nanoTime();
		ByteBuffer mesh = take(FIRST_BUFFER_BYTES);
		int[] counts = new int[REGION_CHUNKS * REGION_CHUNKS];
		long packed = PainiteNative.lodClientMeshRegion(region.rx, region.rz, px, pz, mesh, counts);
		int length = (int) packed;
		if (length > mesh.capacity()) {
			// Too big for this buffer: a larger one, once.
			give(mesh);
			mesh = take(length);
			packed = PainiteNative.lodClientMeshRegion(region.rx, region.rz, px, pz, mesh, counts);
			length = (int) packed;
			if (length > mesh.capacity()) {
				length = 0;
			}
		}
		if (length == 0) {
			give(mesh);
			DONE.add(new Built(region, key, null, 0, null, -64, 320, System.nanoTime() - t0));
			return;
		}
		int[] starts = new int[counts.length + 1];
		for (int i = 0; i < counts.length; i++) {
			starts[i + 1] = starts[i] + counts[i];
		}
		int minY = (short) (packed >>> 32) - 1;
		int maxY = (short) (packed >>> 48) + 1;
		DONE.add(new Built(region, key, mesh, length, starts, minY, maxY, System.nanoTime() - t0));
	}

	private static ByteBuffer take(int atLeast) {
		synchronized (POOL) {
			for (Iterator<ByteBuffer> it = POOL.iterator(); it.hasNext(); ) {
				ByteBuffer b = it.next();
				if (b.capacity() >= atLeast) {
					it.remove();
					b.clear();
					return b;
				}
			}
		}
		return MemoryUtil.memAlloc(Math.max(atLeast, FIRST_BUFFER_BYTES));
	}

	private static void give(ByteBuffer buffer) {
		synchronized (POOL) {
			if (POOL.size() < POOL_IDLE) {
				POOL.add(buffer);
				return;
			}
		}
		MemoryUtil.memFree(buffer);
	}

	/** Render thread: swap the region's buffer for the built mesh. */
	private static void upload(Built built) {
		Region region = built.region();
		region.building = null;
		ByteBuffer mesh = built.mesh();
		if (!REGIONS.containsKey(regionKey(region.rx, region.rz))) {
			if (mesh != null) {
				give(mesh);
			}
			return;
		}
		long t0 = System.nanoTime();
		region.close();
		region.key = built.key();
		if (mesh != null) {
			mesh.position(0).limit(built.length());
			region.buffer = RenderSystem.getDevice().createBuffer(() -> "Painite far view " + region.rx + "," + region.rz, GpuBuffer.USAGE_VERTEX, mesh);
			give(mesh);
			region.vertices = built.length() / VERTEX_BYTES;
			region.starts = built.starts();
			region.minY = built.minY();
			region.maxY = built.maxY();
		}
		rebuilds++;
		buildNs += built.ns();
		uploadNs += System.nanoTime() - t0;
	}

	/** Work that must not run on the render thread (disk reads for the store) shares the mesh worker. */
	public static void offThread(Runnable work) {
		BUILDER.execute(work);
	}

	/** Free every buffer; on the render thread. */
	public static void clear() {
		for (Region region : REGIONS.values()) {
			region.close();
		}
		REGIONS.clear();
		Built built;
		while ((built = DONE.poll()) != null) {
			if (built.mesh() != null) {
				give(built.mesh());
			}
		}
		if (fogBuffer != null) {
			fogBuffer.close();
			fogBuffer = null;
		}
	}

	/** The chunks east of the player to just past the hole: held, lit, covered, as one letter each, for the report. */
	private static String edge = "";

	private static void edgeRow(net.minecraft.client.multiplayer.ClientLevel level, int px, int pz, int hole) {
		StringBuilder out = new StringBuilder("hole ").append(hole);
		int[][] dirs = { { 1, 0, '>' }, { -1, 0, '<' }, { 0, 1, 'v' }, { 0, -1, '^' }, { -1, -1, '\\' } };
		for (int[] dir : dirs) {
			out.append(' ').append((char) dir[2]);
			for (int d = 0; d <= hole + 2; d++) {
				int cx = px + dir[0] * d;
				int cz = pz + dir[1] * d;
				boolean held = level.getChunkSource().hasChunk(cx, cz);
				boolean lit = level.getLightEngine().lightOnInColumn(net.minecraft.core.SectionPos.getZeroNode(cx, cz));
				out.append(held ? (covered(level, cx, cz) ? 'C' : lit ? 'h' : 'd') : '.');
			}
		}
		edge = out.toString();
	}

	public static String report() {
		long held = 0;
		for (Region region : REGIONS.values()) {
			held += region.vertices;
		}
		return String.format("[painite] far view draw: %d regions, %d vertices held, %d rebuilds (%.1f ms mesh off-thread, %.1f ms upload), %d region draws, %d vertices drawn since start, %s",
				REGIONS.size(), held, rebuilds, buildNs / 1e6, uploadNs / 1e6, drawnRegions, drawnVertices, edge);
	}
}
