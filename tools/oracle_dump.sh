#!/usr/bin/env bash
# Dump vanilla surface oracle chunks for rust/terrain/tests/surface_compare.
#
#   CHUNKS="374 46 -120 88 ..." tools/oracle_dump.sh
#
# Starts the dev server with no painite flags on a fresh world
# run/world_oracle$RUN, runs `painite density surface x z` for each
# chunk pair and copies each dump to run/oracles/surface_<x>_<z>.txt
# (and the post-carver dump to carved_<x>_<z>.txt),
# then stops the server. Chunks must be ungenerated: keep them away
# from spawn and from each other. Env: RUN (world suffix), CHUNKS,
# EXTRA (gradle properties, e.g. "-Ppainite.rustTerrain=false" for a
# vanilla dump on a build where the native is default on).
set -euo pipefail
cd "$(dirname "$0")/.."
RUN=${RUN:-}
CHUNKS=${CHUNKS:-"374 46 -300 210 250 -280 -410 -95 520 330 -180 -420 90 470 -520 40 310 -60 -60 300"}
PROPS=run/server.properties
LOGDIR=run/logs
OUT=run/oracles
SERVER_PID=

cleanup() {
	if [[ -n $SERVER_PID ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
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

world="world_oracle$RUN"
if [[ -e run/$world ]]; then
	echo "oracle_dump: run/$world exists, set RUN=<suffix>" >&2
	exit 1
fi
mkdir -p "$OUT"
sed -i "s/^level-name=.*/level-name=$world/" "$PROPS"
t0=$(date +%s)
log="$LOGDIR/oracle_$world.log"
# shellcheck disable=SC2206
extra=(${EXTRA:-})
./gradlew runServer --console=plain -q "${extra[@]}" >"$log" 2>&1 &
gradle_pid=$!
waited=0
until [[ -f $LOGDIR/latest.log ]] && [[ $(stat -c %Y "$LOGDIR/latest.log") -ge $t0 ]] && grep -q "Done (" "$LOGDIR/latest.log"; do
	sleep 2
	waited=$((waited + 2))
	if ! kill -0 "$gradle_pid" 2>/dev/null; then
		echo "oracle_dump: gradle exited before the server was up, see $log" >&2
		exit 1
	fi
	if ((waited > 600)); then
		echo "oracle_dump: server start timeout, see $log" >&2
		kill "$gradle_pid" || true
		exit 1
	fi
done
pid=$(pgrep -n -f "java.*devlaunchinjector" || pgrep -n -f "java.*net.minecraft.server.Main" || true)
if [[ -z $pid ]]; then
	echo "oracle_dump: cannot find the server pid" >&2
	rcon stop >/dev/null || true
	exit 1
fi
SERVER_PID=$pid
echo "oracle_dump: server up after ${waited}s, pid $pid, world $world"

# shellcheck disable=SC2206
pairs=($CHUNKS)
n=0
for ((i = 0; i + 1 < ${#pairs[@]}; i += 2)); do
	x=${pairs[i]} z=${pairs[i + 1]}
	rm -f run/painite_surface_chunk.txt run/painite_carved_chunk.txt
	reply=$(rcon "painite density surface $x $z")
	if [[ -f run/painite_surface_chunk.txt ]]; then
		cp run/painite_surface_chunk.txt "$OUT/surface_${x}_${z}.txt"
		[[ -f run/painite_carved_chunk.txt ]] && cp run/painite_carved_chunk.txt "$OUT/carved_${x}_${z}.txt"
		n=$((n + 1))
		echo "oracle_dump: $x $z -> $(head -1 "$OUT/surface_${x}_${z}.txt")"
	else
		echo "oracle_dump: $x $z produced nothing: $reply" >&2
	fi
done
echo "oracle_dump: $n dumps in $OUT"
rcon stop >/dev/null || true
wait "$gradle_pid" 2>/dev/null || true
SERVER_PID=
