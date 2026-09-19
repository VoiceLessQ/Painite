package me.apika.painite.lod;

import java.util.Arrays;
import me.apika.painite.PainiteMod;
import me.apika.painite.PainiteNative;
import net.minecraft.client.Minecraft;
import net.minecraft.client.gui.components.debug.DebugScreenEntries;
import net.minecraft.client.gui.components.debug.DebugScreenEntryStatus;

/** Per-frame timing of the level render, kept in 10 s windows so a far-view layer has a measured budget to fit. */
public final class FrameProbe {
	public static final boolean ENABLED = Boolean.parseBoolean(System.getProperty("painite.frameProbe", "false"));
	private static final long WINDOW_NS = 10_000_000_000L;
	private static final int CAP = 1 << 14;
	private static final long[] level = new long[CAP];
	private static final long[] frame = new long[CAP];
	private static int count;
	private static long frames;
	private static double gpuSum;
	private static int gpuSamples;
	private static boolean gpuEntryOn;
	private static long windowStart;
	private static long renderStart;
	private static volatile String last = "no window yet";
	/** Seconds between screenshots named painite_farview.png, the first that long after the first frame in a level; 0 for none. */
	private static final int SHOT_AFTER = Integer.getInteger("painite.lodShot", 0);
	private static long firstFrame;

	private FrameProbe() {}

	public static void begin() {
		renderStart = System.nanoTime();
	}

	public static void end() {
		long now = System.nanoTime();
		Minecraft minecraft = Minecraft.getInstance();
		if (windowStart == 0) {
			windowStart = now;
			firstFrame = now;
		}
		if (SHOT_AFTER > 0 && now - firstFrame > SHOT_AFTER * 1_000_000_000L) {
			firstFrame = now;
			net.minecraft.client.Screenshot.grab(minecraft.gameDirectory, "painite_farview.png", minecraft.gameRenderer.mainRenderTarget(), 1, c -> PainiteMod.LOGGER.info("[painite] screenshot: {}", c.getString()));
		}
		if (!gpuEntryOn) {
			// The GPU timer query only runs while this debug entry is enabled; the probe turns it on once.
			gpuEntryOn = true;
			minecraft.debugEntries.setStatus(DebugScreenEntries.GPU_UTILIZATION, DebugScreenEntryStatus.ALWAYS_ON);
		}
		frames++;
		if (count < CAP) {
			level[count] = now - renderStart;
			frame[count] = minecraft.getFrameTimeNs();
			count++;
		}
		double gpu = minecraft.getGpuUtilization();
		if (gpu > 0) {
			gpuSum += gpu;
			gpuSamples++;
		}
		if (now - windowStart >= WINDOW_NS) {
			last = summarize(now - windowStart);
			PainiteMod.LOGGER.info(last);
			if (LodRenderer.ENABLED) {
				PainiteMod.LOGGER.info(LodRenderer.report());
			}
			count = 0;
			frames = 0;
			gpuSum = 0;
			gpuSamples = 0;
			windowStart = now;
		}
	}

	private static String summarize(long spanNs) {
		int n = count;
		if (n == 0) {
			return "[painite] frame: no frames in window";
		}
		long[] lv = Arrays.copyOf(level, n);
		long[] fr = Arrays.copyOf(frame, n);
		Arrays.sort(lv);
		Arrays.sort(fr);
		int held = PainiteNative.AVAILABLE ? PainiteNative.lodClientCount() : 0;
		Minecraft minecraft = Minecraft.getInstance();
		double gpu = gpuSamples == 0 ? 0 : gpuSum / gpuSamples;
		return String.format("[painite] frame: %d frames in %.1f s (%.0f fps, %d sampled), %dx%d, gpu %.0f%%, level render p50 %.2f p95 %.2f p99 %.2f max %.2f ms, whole frame p50 %.2f p95 %.2f max %.2f ms, %d far-view records held",
				frames, spanNs / 1e9, frames * 1e9 / spanNs, n, minecraft.getWindow().getWidth(), minecraft.getWindow().getHeight(), gpu,
				ms(lv, 0.50), ms(lv, 0.95), ms(lv, 0.99), lv[n - 1] / 1e6,
				ms(fr, 0.50), ms(fr, 0.95), fr[n - 1] / 1e6, held);
	}

	private static double ms(long[] sorted, double q) {
		int i = Math.min(sorted.length - 1, (int) (q * sorted.length));
		return sorted[i] / 1e6;
	}

	public static String report() {
		return last;
	}
}
