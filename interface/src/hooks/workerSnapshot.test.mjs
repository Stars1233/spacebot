import {describe, expect, test} from "bun:test";
import {reconcileWorkerSnapshot, resumeWorker, workerLifecycleKey} from "./workerSnapshot.ts";

function reconcile(current, snapshot, requestGeneration, lifecycleGenerations) {
	return reconcileWorkerSnapshot(
		current,
		snapshot,
		requestGeneration,
		lifecycleGenerations,
		(worker) => worker.agentId === "agent-a",
		(worker) => workerLifecycleKey(worker.agentId, worker.id),
		(worker) => worker.id,
		(worker) => workerLifecycleKey(worker.agentId, worker.id),
		(_current, worker) => worker,
	);
}

describe("worker snapshot reconciliation", () => {
	test("does not resurrect a worker completed during the request", () => {
		const staleWorker = {id: "worker-a", registrationId: "1", agentId: "agent-a"};
		const generations = new Map([[workerLifecycleKey("agent-a", "worker-a"), 2]]);

		expect(reconcile({}, [staleWorker], 1, generations)).toEqual({});
	});

	test("preserves a replacement registration created during the request", () => {
		const replacement = {id: "worker-a", registrationId: "2", agentId: "agent-a"};
		const staleWorker = {id: "worker-a", registrationId: "1", agentId: "agent-a"};
		const generations = new Map([[workerLifecycleKey("agent-a", "worker-a"), 2]]);

		expect(reconcile({"worker-a": replacement}, [staleWorker], 1, generations)).toEqual({
			"worker-a": replacement,
		});
	});
});

describe("worker resume", () => {
	function idleWorker() {
		return {
			id: "worker-a",
			registrationId: "1",
			agentId: "agent-a",
			isIdle: true,
			runtimeState: "waiting_for_input",
			routable: true,
		};
	}

	test("returns an idle worker to running and records its lifecycle", () => {
		let recorded = 0;

		expect(resumeWorker(idleWorker(), () => (recorded += 1))).toEqual({
			...idleWorker(),
			isIdle: false,
			runtimeState: "running",
			routable: false,
		});
		expect(recorded).toBe(1);
	});

	test("leaves a running worker untouched without recording", () => {
		const runningWorker = {...idleWorker(), isIdle: false, runtimeState: "running"};
		let recorded = 0;

		expect(resumeWorker(runningWorker, () => (recorded += 1))).toBe(runningWorker);
		expect(recorded).toBe(0);
	});

	test("a snapshot requested while idle does not restore idle after a resume", () => {
		const generations = new Map();
		let generation = 0;
		const requestGeneration = generation;
		const resumed = resumeWorker(idleWorker(), () => {
			generation += 1;
			generations.set(workerLifecycleKey("agent-a", "worker-a"), generation);
		});

		expect(
			reconcile({"worker-a": resumed}, [idleWorker()], requestGeneration, generations),
		).toEqual({"worker-a": resumed});
	});
});
