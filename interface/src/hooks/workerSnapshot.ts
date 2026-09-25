export function workerLifecycleKey(scopeId: string, workerId: string): string {
	return `${scopeId}:${workerId}`;
}

export function reconcileWorkerSnapshot<TCurrent, TSnapshot>(
	current: Record<string, TCurrent>,
	snapshot: TSnapshot[],
	requestGeneration: number,
	lifecycleGenerations: ReadonlyMap<string, number>,
	belongsToScope: (worker: TCurrent) => boolean,
	currentKey: (worker: TCurrent) => string,
	snapshotId: (worker: TSnapshot) => string,
	snapshotKey: (worker: TSnapshot) => string,
	merge: (current: TCurrent | undefined, snapshot: TSnapshot) => TCurrent,
): Record<string, TCurrent> {
	const next = {...current};
	for (const [workerId, worker] of Object.entries(current)) {
		if (
			belongsToScope(worker) &&
			(lifecycleGenerations.get(currentKey(worker)) ?? 0) <= requestGeneration
		) {
			delete next[workerId];
		}
	}
	for (const worker of snapshot) {
		const workerId = snapshotId(worker);
		if ((lifecycleGenerations.get(snapshotKey(worker)) ?? 0) > requestGeneration) continue;
		next[workerId] = merge(current[workerId], worker);
	}
	return next;
}

interface WorkerRuntimeFields {
	isIdle: boolean;
	runtimeState: string;
	routable: boolean;
}

/**
 * Return an idle worker to the running state. Workers emit tool calls, model
 * text, and OpenCode parts only while an operation is running, so any of them
 * arriving after `worker_idle` means a routed follow-up resumed the worker.
 * `worker_status` is not a resume signal: OpenCode workers emit it while
 * waiting for input. `recordLifecycle` runs only when the worker is resumed,
 * so a snapshot requested while it was idle cannot restore the idle state.
 * Returns the same object when the worker is not idle.
 */
export function resumeWorker<T extends WorkerRuntimeFields>(
	worker: T,
	recordLifecycle: () => void,
): T {
	if (worker.runtimeState !== "waiting_for_input") return worker;
	recordLifecycle();
	return {...worker, isIdle: false, runtimeState: "running", routable: false};
}
