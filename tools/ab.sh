#!/usr/bin/env bash
# Burst A/B driver for the dev server (warm protocol, LOCAL_DESIGN
# "Warm protocol results").
#
#   ARMS="off rt fr4" tools/ab.sh
#
# Arm letters: f parallelFeatures, s parallelFeatures+parallelStructures,
# r rustTerrain, o rustOres, p fastPack; trailing digits set
# painite.workers. "off" and "offN" run vanilla serial ("offp": serial
# plus fastPack). Each arm gets a fresh world
# run/world_<arm>$RUN (the script refuses to reuse one).
#
# Per arm: start `gradlew runServer`, wait for Done, randomTickSpeed 0,
# forceload four 16x16-chunk squares at +-1008 (warm), sleep $WARM,
# unload, probe reset, forceload the same squares at +-3008, poll
# `painite probe report` every $POLL s until the `full` job count sits
# still for $STILL polls, then report and stop. CPU seconds come from
# /proc/<pid>/stat between the burst forceload and the last change.
#
# Env: RUN (world suffix), WARM (45), WARM2 (1: a second warm set at
# +-2032 before measuring; 0 leaves C2 compiling inside the burst),
# POLL (2), STILL (3), BURST_TIMEOUT (240), JFR=<file> records the run,
# EXTRA="-Ppainite.vmArgs=..." passes gradle properties through (space
# separated, no quoting inside).
#
# MODE=fly replaces the burst with a player-shaped load: FLY_STRIPS (60)
# strips of FLY_STEP (2) x (FLY_Z1-FLY_Z0+1) chunks, one per second along
# +x from chunk FLY_CX0 (188), z chunks FLY_Z0..FLY_Z1 (-10..10), strips
# older than FLY_KEEP (12) removed. Reports CPU per full chunk, peak /
# median cores per second over the flight, and the flight's demand
# against the full-chunk rate (ab_<world>.fly.full: second, full count).
# Spectator cap: flyingSpeed 0.2 sprinting gives 4.04 blocks/tick, 5
# chunks/s; view 32 is FLY_STEP=5 FLY_Z0=-32 FLY_Z1=32 FLY_KEEP=13.
set -euo pipefail
MODE=${MODE:-burst}
FLY_STRIPS=${FLY_STRIPS:-60}
FLY_KEEP=${FLY_KEEP:-12}
FLY_STEP=${FLY_STEP:-2}
FLY_CX0=${FLY_CX0:-188}
FLY_Z0=${FLY_Z0:--10}
FLY_Z1=${FLY_Z1:-10}
cd "$(dirname "$0")/.."
ARMS=${ARMS:-off}
RUN=${RUN:-}
WARM=${WARM:-45}
POLL=${POLL:-2}
STILL=${STILL:-3}
BURST_TIMEOUT=${BURST_TIMEOUT:-240}
JFR=${JFR:-}
EXTRA=${EXTRA:-}
PROPS=run/server.properties
LOGDIR=run/logs
TARGET_CHUNKS=1024
CLK_TCK=$(getconf CLK_TCK)
SERVER_PID=

# A failed arm must not leave the server (and its port) behind.
cleanup() {
	if [[ -n $SERVER_PID ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
		echo "ab: stopping server $SERVER_PID" >&2
		rcon stop >/dev/null 2>&1 || kill "$SERVER_PID" 2>/dev/null || true
		local n=0
		while kill -0 "$SERVER_PID" 2>/dev/null && ((n < 60)); do
			sleep 2
			n=$((n + 2))
		done
		kill -0 "$SERVER_PID" 2>/dev/null && kill -9 "$SERVER_PID" 2>/dev/null || true
	fi
}
trap cleanup EXIT

rcon() { python3 tools/rcon.py --props "$PROPS" "$@"; }

cpu_ticks() { awk '{split($0, a, ") "); n = split(a[2], f, " "); print f[12] + f[13]}' "/proc/$1/stat"; }

# "tid comm ticks" per thread of a pid (comm may contain spaces).
thread_ticks() {
	local t
	for t in /proc/"$1"/task/*; do
		[[ -r $t/stat ]] || continue
		awk -v tid="${t##*/}" '{c = $0; sub(/^[0-9]+ \(/, "", c); sub(/\).*/, "", c); split($0, a, ") "); split(a[2], f, " "); print tid, c, f[12] + f[13]}' "$t/stat" 2>/dev/null
	done
}

# Top threads by CPU seconds between two thread_ticks snapshots.
top_threads() {
	awk -v tck="$CLK_TCK" 'NR == FNR {a[$1] = $NF; next} ($1 in a) {d = $NF - a[$1]; if (d > 0) {name = $0; sub(/^[0-9]+ /, "", name); sub(/ [0-9]+$/, "", name); printf "%.2f %s\n", d / tck, name}}' "$1" "$2" | sort -rn | head -8
}

# Four 16x16-chunk squares with corners at +-a blocks (a multiple of 16).
# Negative sides use -(a+256)..-(a+1) so they stay 16 chunks after
# floor division; -(a+255)..-a would be 17 and trip forceload's limit.
squares() {
	local a=$1
	local b=$((a + 255)) nlo=$((a + 256)) nhi=$((a + 1))
	rcon "forceload add $a $a $b $b" "forceload add -$nlo -$nlo -$nhi -$nhi" \
		"forceload add $a -$nlo $b -$nhi" "forceload add -$nlo $a -$nhi $b" | grep -v "^Marked" >&2 || true
}

# One strip per second along +x; per-second CPU ticks of the server go to $2.
fly_strips() {
	local pid=$1 out=$2 i cx ox prev cur z zb rep
	local z0=$((FLY_Z0 * 16)) z1=$((FLY_Z1 * 16 + 15))
	# forceload takes at most 256 chunks per command; split a strip along z.
	local zrows=$((256 / FLY_STEP))
	: >"$out"
	: >"$out.full"
	prev=$(cpu_ticks "$pid")
	for ((i = 0; i < FLY_STRIPS; i++)); do
		cx=$((FLY_CX0 + FLY_STEP * i))
		for ((z = FLY_Z0; z <= FLY_Z1; z += zrows)); do
			zb=$((z + zrows - 1)); ((zb > FLY_Z1)) && zb=$FLY_Z1
			rcon "forceload add $((cx * 16)) $((z * 16)) $((cx * 16 + FLY_STEP * 16 - 1)) $((zb * 16 + 15))" >/dev/null
		done
		if ((i >= FLY_KEEP)); then
			ox=$((FLY_CX0 + FLY_STEP * (i - FLY_KEEP)))
			for ((z = FLY_Z0; z <= FLY_Z1; z += zrows)); do
				zb=$((z + zrows - 1)); ((zb > FLY_Z1)) && zb=$FLY_Z1
				rcon "forceload remove $((ox * 16)) $((z * 16)) $((ox * 16 + FLY_STEP * 16 - 1)) $((zb * 16 + 15))" >/dev/null
			done
		fi
		sleep 1
		cur=$(cpu_ticks "$pid")
		echo $((cur - prev)) >>"$out"
		prev=$cur
		rep=$(rcon "painite probe report")
		echo "$((i + 1)) $(full_jobs "$rep" | head -1)" >>"$out.full"
	done
}

full_jobs() { echo "$1" | awk '$1 == "full" || $1 == "minecraft:full" {for (i = 2; i <= NF; i++) if ($i ~ /^jobs=/) {sub("jobs=", "", $i); print $i}}'; }

arm_props() {
	local arm=$1 props=()
	# "off", "off2", "offp": vanilla serial; letters after "off" still parse.
	[[ $arm == off* ]] && arm=${arm#off}
	# parallelFeatures and rustTerrain default on in the mod; arms state them either way.
	if [[ $arm == *f* || $arm == *s* ]]; then props+=(-Ppainite.parallelFeatures=true); else props+=(-Ppainite.parallelFeatures=false); fi
	[[ $arm == *s* ]] && props+=(-Ppainite.parallelStructures=true)
	if [[ $arm == *r* ]]; then props+=(-Ppainite.rustTerrain=true); else props+=(-Ppainite.rustTerrain=false); fi
	[[ $arm == *o* ]] && props+=(-Ppainite.rustOres=true)
	[[ $arm == *p* ]] && props+=(-Ppainite.fastPack=true)
	local workers=${arm//[^0-9]/}
	[[ -n $workers ]] && props+=(-Ppainite.workers="$workers")
	[[ -n $JFR ]] && props+=(-Ppainite.jfr="$JFR")
	printf '%s\n' "${props[@]}"
}

run_arm() {
	local arm=$1 world="world_${arm}${RUN}"
	if [[ -e run/$world ]]; then
		echo "ab: run/$world exists, set RUN=<suffix>" >&2
		return 1
	fi
	local props
	mapfile -t props < <(arm_props "$arm")
	# shellcheck disable=SC2206
	local extra=($EXTRA)
	sed -i "s/^level-name=.*/level-name=$world/" "$PROPS"
	local t0 log="$LOGDIR/ab_$world.log" report="$LOGDIR/ab_$world.report"
	t0=$(date +%s)
	echo "ab: $arm -> $world, props: ${props[*]:-none} ${extra[*]:-}"
	./gradlew runServer --console=plain -q "${props[@]}" "${extra[@]}" >"$log" 2>&1 &
	local gradle_pid=$!
	local waited=0
	until [[ -f $LOGDIR/latest.log ]] && [[ $(stat -c %Y "$LOGDIR/latest.log") -ge $t0 ]] && grep -q "Done (" "$LOGDIR/latest.log"; do
		sleep 2
		waited=$((waited + 2))
		if ! kill -0 "$gradle_pid" 2>/dev/null; then
			echo "ab: gradle exited before the server was up, see $log" >&2
			return 1
		fi
		if ((waited > 600)); then
			echo "ab: server start timeout, see $log" >&2
			kill "$gradle_pid" || true
			return 1
		fi
	done
	local pid
	pid=$(pgrep -n -f "java.*devlaunchinjector" || pgrep -n -f "java.*net.minecraft.server.Main" || true)
	if [[ -z $pid ]]; then
		echo "ab: cannot find the server pid" >&2
		rcon stop >/dev/null || true
		return 1
	fi
	SERVER_PID=$pid
	echo "ab: server up after ${waited}s, pid $pid"
	rcon "gamerule randomTickSpeed 0" >/dev/null
	squares 1008
	sleep "$WARM"
	if [[ ${WARM2:-1} == 1 ]]; then
		# Second warm set at +-2032 (chunks 127..142) so C2 finishes before the
		# burst; one set leaves C2 as the top thread and halves chunks/s.
		rcon "forceload remove all" >/dev/null
		squares 2032
		sleep "$WARM"
	fi
	# Let the warm chunks unload and save before measuring: with 5 s their
	# serialisation (PalettedContainer.pack) showed up inside the burst.
	rcon "forceload remove all" >/dev/null
	sleep "${WARM_DRAIN:-30}"
	rcon "painite probe reset" >/dev/null
	sleep 2
	local c0 c_last t_burst t_last last=0 still=0 elapsed=0 rep
	c0=$(cpu_ticks "$pid")
	thread_ticks "$pid" >"$LOGDIR/ab_$world.t0"
	t_burst=$(date +%s.%N)
	if [[ $MODE == fly ]]; then
		fly_strips "$pid" "$LOGDIR/ab_$world.fly"
	else
		# Per-second CPU ticks of the server for 30 s, started before the
		# forceloads because each of the four takes about a second to return.
		(
			prev=$(cpu_ticks "$pid")
			for _ in $(seq 30); do
				sleep 1
				cur=$(cpu_ticks "$pid" 2>/dev/null) || break
				echo $((cur - prev))
				prev=$cur
			done >"$LOGDIR/ab_$world.fly"
		) &
		squares 3008
	fi
	c_last=$c0
	t_last=$t_burst
	while ((elapsed < BURST_TIMEOUT)); do
		sleep "$POLL"
		elapsed=$((elapsed + POLL))
		rep=$(rcon "painite probe report")
		local full
		full=$(full_jobs "$rep")
		full=${full:-0}
		if ((full > last)); then
			last=$full
			still=0
			c_last=$(cpu_ticks "$pid")
			thread_ticks "$pid" >"$LOGDIR/ab_$world.t1"
			t_last=$(date +%s.%N)
		elif ((last > 0)); then
			still=$((still + 1))
			((still >= STILL)) && break
		fi
	done
	local cpu_s span
	cpu_s=$(awk -v a="$c0" -v b="$c_last" -v t="$CLK_TCK" 'BEGIN {printf "%.2f", (b - a) / t}')
	span=$(awk -v a="$t_burst" -v b="$t_last" 'BEGIN {printf "%.1f", b - a}')
	{
		echo "arm $arm world $world props ${props[*]:-none} ${extra[*]:-}"
		echo "burst: full=$last span=${span}s cpu=${cpu_s}s cpu_ms_per_chunk=$(awk -v c="$cpu_s" -v n="$TARGET_CHUNKS" 'BEGIN {printf "%.1f", c * 1000 / n}') (per $TARGET_CHUNKS target chunks)"
		if [[ -s $LOGDIR/ab_$world.fly.full ]]; then
			local demand
			demand=$((FLY_STEP * (FLY_Z1 - FLY_Z0 + 1)))
			awk -v d="$demand" -v n="$FLY_STRIPS" 'END {printf "flight: demand=%d c/s over %d s (%d chunks), full at flight end=%d (%.0f c/s), backlog=%d\n", d, n, d * n, $2, $2 / $1, d * n - $2}' "$LOGDIR/ab_$world.fly.full"
		fi
		if [[ -s $LOGDIR/ab_$world.fly ]]; then
			echo "per second: cpu_ms_per_full_chunk=$(awk -v c="$cpu_s" -v n="$last" 'BEGIN {printf "%.1f", n ? c * 1000 / n : 0}') cores/s peak=$(sort -n "$LOGDIR/ab_$world.fly" | tail -1 | awk -v t="$CLK_TCK" '{printf "%.1f", $1 / t}') median=$(sort -n "$LOGDIR/ab_$world.fly" | awk -v t="$CLK_TCK" '{a[NR] = $1} END {printf "%.1f", a[int((NR + 1) / 2)] / t}')"
		fi
		echo "$rep"
		if [[ -s $LOGDIR/ab_$world.t1 ]]; then
			echo "top threads by CPU seconds over the burst:"
			top_threads "$LOGDIR/ab_$world.t0" "$LOGDIR/ab_$world.t1"
		fi
	} | tee "$report"
	rcon stop >/dev/null || true
	local down=0
	while kill -0 "$pid" 2>/dev/null && ((down < 90)); do
		sleep 2
		down=$((down + 2))
	done
	if kill -0 "$pid" 2>/dev/null; then
		echo "ab: server did not stop, killing $pid" >&2
		kill "$pid" || true
	fi
	wait "$gradle_pid" 2>/dev/null || true
	SERVER_PID=
	echo "ab: $arm done, report in $report"
}

for arm in $ARMS; do
	run_arm "$arm"
done
