package me.apika.painite;

import me.apika.painite.command.PainiteCommand;
import me.apika.painite.lod.LodPayloads;
import me.apika.painite.lod.LodSender;
import me.apika.painite.terrain.TerrainBridge;
import me.apika.painite.probe.DispatcherProbe;
import me.apika.painite.probe.StageProbe;
import me.apika.painite.probe.StepProbe;
import me.apika.painite.sched.PainitePool;
import me.apika.painite.sched.StageGate;
import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.command.v2.CommandRegistrationCallback;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public class PainiteMod implements ModInitializer {
	public static final String MOD_ID = "painite";
	public static final Logger LOGGER = LoggerFactory.getLogger(MOD_ID);

	@Override
	public void onInitialize() {
		int workers = PainitePool.workerCount();
		boolean ready = PainiteNative.AVAILABLE && PainiteNative.init(workers, workers * 2) >= 0;
		LOGGER.info("[painite] native={} workers={} parallelFeatures={} parallelStructures={}",
				ready, workers, StageGate.PARALLEL_FEATURES, StageGate.PARALLEL_STRUCTURES);
		if (!ready && (StageGate.PARALLEL_FEATURES || StageGate.PARALLEL_STRUCTURES)) {
			LOGGER.warn("[painite] native missing: stage gates disabled, worldgen runs vanilla-serial");
		}
		CommandRegistrationCallback.EVENT.register((dispatcher, registry, env) -> PainiteCommand.register(dispatcher));
		TerrainBridge.register();
		LodPayloads.register();
		if (TerrainBridge.LOD) {
			LodSender.register();
			me.apika.painite.lod.LodRefresh.register();
			me.apika.painite.lod.LodBackfill.register();
		}
		ServerLifecycleEvents.SERVER_STOPPING.register(server -> LOGGER.info(StageProbe.report() + DispatcherProbe.report() + StepProbe.report()));
		ServerLifecycleEvents.SERVER_STOPPING.register(server -> LOGGER.info(LodSender.report()));
	}
}
