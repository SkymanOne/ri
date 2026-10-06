// `@earendil-works/pi-agent-core` for extensions in yapi. yapi runs the agent loop
// natively, so these exports exist only so imports link; each throws when used.
export const { Agent, agentLoop, agentLoopContinue, runAgentLoop, runAgentLoopContinue, runToolCall, setDefaultStreamFn, streamProxy } =
	globalThis.__yapi.stubs("@earendil-works/pi-agent-core");
