// A Pi extension that runs unchanged in yapi and in Pi: a tool, a command,
// a flag and an event handler. Copy it, rename the parts and replace the bodies.
import { Type } from "typebox";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.registerFlag("shout-suffix", {
		type: "string",
		default: "!",
		description: "Text after shouted words",
	});

	pi.registerTool({
		name: "shout",
		label: "Shout",
		description: "Repeats the text in capitals",
		promptSnippet: "Shout text back in capitals",
		parameters: Type.Object({ text: Type.String({ description: "What to shout" }) }),
		async execute(_toolCallId, params: { text: string }) {
			const text = params.text.toUpperCase();
			const suffix = String(pi.getFlag("shout-suffix") ?? "");
			return {
				content: [{ type: "text", text: `${text}${suffix}` }],
				details: { length: text.length },
			};
		},
	});

	pi.registerCommand("hello", {
		description: "Says hello",
		handler: async (args, ctx) => {
			ctx.ui.notify(`Hello, ${args || "world"}!`, "info");
		},
	});

	pi.on("tool_call", (event) => {
		if (event.toolName === "shout" && event.input.text === "") {
			return { block: true, reason: "Nothing to shout" };
		}
	});
}
