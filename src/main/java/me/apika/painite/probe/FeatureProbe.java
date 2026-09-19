package me.apika.painite.probe;

import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.LongAdder;
import net.minecraft.core.registries.Registries;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.world.level.levelgen.placement.PlacedFeature;

/** Busy time per placed feature of the decoration pass, -Dpainite.featureProbe=true. */
public final class FeatureProbe {
	public static final boolean ENABLED = Boolean.getBoolean("painite.featureProbe");
	private static final int TOP = 25;

	private static volatile Map<PlacedFeature, FeatureProbe> byFeature = Map.of();
	private static final FeatureProbe INLINE = new FeatureProbe("(unregistered)");

	private final String name;
	private final LongAdder calls = new LongAdder();
	private final LongAdder placed = new LongAdder();
	private final LongAdder nanos = new LongAdder();

	private FeatureProbe(String name) {
		this.name = name;
	}

	/** Filled once and only read after, so worker threads share it without a lock. */
	public static void install(ServerLevel level) {
		if (!ENABLED) {
			return;
		}
		Map<PlacedFeature, FeatureProbe> map = new IdentityHashMap<>();
		level.registryAccess().lookupOrThrow(Registries.PLACED_FEATURE).listElements()
				.forEach(holder -> map.put(holder.value(), new FeatureProbe(holder.key().identifier().toString())));
		byFeature = map;
	}

	public static FeatureProbe of(PlacedFeature feature) {
		FeatureProbe p = byFeature.get(feature);
		return p == null ? INLINE : p;
	}

	public void stop(long t0, boolean result) {
		nanos.add(System.nanoTime() - t0);
		calls.increment();
		if (result) {
			placed.increment();
		}
	}

	public static String report() {
		if (!ENABLED) {
			return "";
		}
		List<FeatureProbe> all = new ArrayList<>(byFeature.values());
		all.add(INLINE);
		all.removeIf(p -> p.calls.sum() == 0);
		all.sort((a, b) -> Long.compare(b.nanos.sum(), a.nanos.sum()));
		double total = 0;
		for (FeatureProbe p : all) {
			total += p.nanos.sum() / 1e9;
		}
		StringBuilder sb = new StringBuilder(String.format("[painite] feature probe: %d features, busy=%.2fs%n", all.size(), total));
		double rest = 0;
		for (int i = 0; i < all.size(); i++) {
			FeatureProbe p = all.get(i);
			double busy = p.nanos.sum() / 1e9;
			if (i >= TOP) {
				rest += busy;
				continue;
			}
			long n = p.calls.sum();
			sb.append(String.format("  %-44s calls=%d placed=%d busy=%.2fs mean=%.1fus%n", p.name, n, p.placed.sum(), busy, busy * 1e6 / n));
		}
		if (all.size() > TOP) {
			sb.append(String.format("  %-44s busy=%.2fs%n", "(other " + (all.size() - TOP) + ")", rest));
		}
		return sb.toString();
	}

	public static void reset() {
		for (FeatureProbe p : byFeature.values()) {
			p.clear();
		}
		INLINE.clear();
	}

	private void clear() {
		calls.reset();
		placed.reset();
		nanos.reset();
	}
}
