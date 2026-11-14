// Written by `triage hooks install --pi`; `triage hooks uninstall --pi` removes it.
// Delivers triage peer mail into this pi session.
import { spawn, type ChildProcess } from "node:child_process";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const TRIAGE = __TRIAGE__;
const HOOK_VERSION = "--triage-hook=v1";
const MIN_RETRY_MS = 1_000;
const MAX_RETRY_MS = 60_000;

export default function (pi: ExtensionAPI) {
	let session: string | undefined;
	let waiter: ChildProcess | undefined;
	let retry: ReturnType<typeof setTimeout> | undefined;
	let retryMs = MIN_RETRY_MS;

	const hook = (event: string, id: string) => {
		const child = spawn(TRIAGE, ["inbox", "--hook", "pi", event, HOOK_VERSION], {
			stdio: ["pipe", "pipe", "ignore"],
		});
		child.on("error", () => {});
		child.stdin?.on("error", () => {});
		child.stdin?.end(JSON.stringify({ session_id: id }));
		return child;
	};

	const stop = () => {
		session = undefined;
		if (retry) clearTimeout(retry);
		retry = undefined;
		waiter?.kill();
		waiter = undefined;
	};

	// The waiter blocks until mail arrives, prints it and exits; anything
	// else (another session took over, an error) exits with no output.
	const wait = (id: string) => {
		retry = undefined;
		if (session !== id) return;
		const started = Date.now();
		const child = hook("wait", id);
		waiter = child;
		let out = "";
		child.stdout?.setEncoding("utf8");
		child.stdout?.on("data", (chunk: string) => {
			out += chunk;
		});
		child.on("close", () => {
			if (waiter !== child || session !== id) return;
			waiter = undefined;
			const text = out.trim();
			if (text) {
				retryMs = MIN_RETRY_MS;
				pi.sendMessage(
					{ customType: "triage-mail", content: text, display: true },
					{ deliverAs: "steer", triggerTurn: true },
				);
				wait(id);
				return;
			}
			if (Date.now() - started > MAX_RETRY_MS) retryMs = MIN_RETRY_MS;
			retry = setTimeout(() => wait(id), retryMs);
			retryMs = Math.min(retryMs * 2, MAX_RETRY_MS);
		});
	};

	pi.on("session_start", async (_event, ctx) => {
		stop();
		const id = ctx.sessionManager.getSessionId();
		session = id;
		hook("session-start", id).on("close", () => wait(id));
	});

	pi.on("session_shutdown", async () => {
		stop();
	});
}
