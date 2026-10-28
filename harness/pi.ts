// drover's pi extension. Inert unless pi starts with --drover-hook <url>, which drover's adapter
// (src/harness/pi.rs) passes. It posts to drover what pi's rpc mode does not say or a TUI never
// does, and holds a tool call that changes something until drover answers.
import http from "node:http";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const READ_ONLY = new Set(["read", "grep", "find", "ls"]);
// a reader runs these and nothing else; shell syntax would smuggle in more
const BRAIN = /^brain (find|get|pages|code)( [^;&|`$<>()\n]*)?$/;

function post(url: string, body: unknown): Promise<any> {
	return new Promise((resolve, reject) => {
		const req = http.request(url, { method: "POST", headers: { "content-type": "application/json" } }, (res) => {
			let data = "";
			res.on("data", (c) => (data += c));
			res.on("end", () => {
				try {
					resolve(JSON.parse(data));
				} catch (e) {
					reject(e);
				}
			});
		});
		req.on("error", reject);
		req.end(JSON.stringify(body));
	});
}

function text(m: any): string {
	return (m?.content ?? [])
		.filter((b: any) => b.type === "text")
		.map((b: any) => b.text)
		.join("");
}

export default function (pi: ExtensionAPI) {
	pi.registerFlag("drover-hook", { description: "drover: URL to post events and permissions to", type: "string" });
	pi.registerFlag("drover-reader", { description: "drover: allow only read and brain commands", type: "boolean" });

	const url = pi.getFlag("drover-hook") as string | undefined;
	if (!url) return;
	const reader = pi.getFlag("drover-reader") === true;

	let last: any;
	const tell = (ctx: any, event: string, extra: object = {}) =>
		post(url, { event, session_id: ctx.sessionManager.getSessionId(), ...extra }).catch(() => undefined);
	// the TUI has no stream drover could read, so a pane reports all; rpc reports only these two
	const watch = (name: string, f: (e: any) => object) =>
		pi.on(name as any, async (e: any, ctx: any) => {
			if (ctx.mode === "tui") await tell(ctx, name, f(e));
		});

	pi.on("session_start", async (_e, ctx) => {
		await tell(ctx, "session_start");
	});
	watch("agent_start", () => ({}));
	watch("tool_execution_start", (e) => ({ tool: e.toolName }));
	watch("tool_execution_end", (e) => ({ tool: e.toolName, ok: !e.isError }));
	watch("session_shutdown", (e) => ({ reason: e.reason }));
	pi.on("agent_end", async (e) => {
		last = [...e.messages].reverse().find((m: any) => m.role === "assistant");
	});
	watch("agent_settled", () => ({ text: text(last), error: last?.stopReason === "error" }));

	pi.on("tool_call", async (e, ctx) => {
		const command = (e.input as any).command;
		if (reader) {
			const ok = READ_ONLY.has(e.toolName) || (e.toolName === "bash" && BRAIN.test(String(command).trim()));
			return ok ? undefined : { block: true, reason: "a reader may only read and run brain find|get|pages|code" };
		}
		if (READ_ONLY.has(e.toolName)) return undefined;
		try {
			const r = await post(url, {
				event: "permission",
				session_id: ctx.sessionManager.getSessionId(),
				tool: e.toolName,
				input: e.input,
			});
			return r.allow ? undefined : { block: true, reason: r.reason ?? "denied by the user" };
		} catch {
			return { block: true, reason: "drover is not reachable, so the call was not approved" };
		}
	});
}
