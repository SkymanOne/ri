// `@earendil-works/pi-agent-core` for extensions in ri. ri runs the agent loop
// natively, so these exports exist only so imports link; each throws when used.
function unavailable(name) {
	const error = new Error(`${name} from @earendil-works/pi-agent-core is not available in ri extensions`);
	error.code = "ERR_NOT_SUPPORTED";
	throw error;
}

export class Agent {
	constructor() {
		unavailable("Agent");
	}
}
export const agentLoop = () => unavailable("agentLoop");
export const agentLoopContinue = () => unavailable("agentLoopContinue");
export const runAgentLoop = () => unavailable("runAgentLoop");
export const runAgentLoopContinue = () => unavailable("runAgentLoopContinue");
export const runToolCall = () => unavailable("runToolCall");
export const setDefaultStreamFn = () => unavailable("setDefaultStreamFn");
export const streamProxy = () => unavailable("streamProxy");
